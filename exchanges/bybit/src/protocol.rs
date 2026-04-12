use poise_core::types::Side;
use poise_engine::ports::OrderStatus;
use serde::{Deserialize, Deserializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BybitOrderStatus {
    New,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
    Expired,
    Untriggered,
    Triggered,
    Deactivated,
}

impl BybitOrderStatus {
    pub(crate) fn into_order_status(self) -> Option<OrderStatus> {
        match self {
            Self::New => Some(OrderStatus::New),
            Self::PartiallyFilled => Some(OrderStatus::PartiallyFilled),
            Self::Filled => Some(OrderStatus::Filled),
            Self::Cancelled => Some(OrderStatus::Canceled),
            Self::Rejected => Some(OrderStatus::Rejected),
            Self::Expired => Some(OrderStatus::Expired),
            Self::Untriggered | Self::Triggered | Self::Deactivated => None,
        }
    }

    pub(crate) fn is_trackable(self) -> bool {
        self.into_order_status().is_some()
    }
}

pub(crate) fn deserialize_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(value) => value
            .as_f64()
            .ok_or_else(|| Error::custom("expected finite decimal number")),
        serde_json::Value::String(value) => value
            .trim()
            .parse::<f64>()
            .map_err(|error| Error::custom(format!("invalid decimal `{value}`: {error}"))),
        other => Err(Error::custom(format!(
            "expected decimal string or number, got {other}"
        ))),
    }
}

pub(crate) fn deserialize_optional_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(value)) => value
            .as_f64()
            .ok_or_else(|| Error::custom("expected finite decimal number"))
            .map(Some),
        Some(serde_json::Value::String(value)) => {
            let value = value.trim();
            if value.is_empty() {
                Ok(None)
            } else {
                value
                    .parse::<f64>()
                    .map(Some)
                    .map_err(|error| Error::custom(format!("invalid decimal `{value}`: {error}")))
            }
        }
        Some(other) => Err(Error::custom(format!(
            "expected optional decimal string or number, got {other}"
        ))),
    }
}

pub(crate) fn deserialize_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::Number(value) => value
            .as_i64()
            .ok_or_else(|| Error::custom("expected integer number")),
        serde_json::Value::String(value) => value
            .trim()
            .parse::<i64>()
            .map_err(|error| Error::custom(format!("invalid integer `{value}`: {error}"))),
        other => Err(Error::custom(format!("expected integer, got {other}"))),
    }
}

pub(crate) fn deserialize_side<'de, D>(deserializer: D) -> Result<Side, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let value = String::deserialize(deserializer)?;
    parse_side(&value).ok_or_else(|| Error::custom(format!("unsupported Bybit side: {value}")))
}

pub(crate) fn deserialize_optional_side<'de, D>(deserializer: D) -> Result<Option<Side>, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref().map(str::trim) {
        None | Some("") => Ok(None),
        Some(value) => parse_side(value)
            .map(Some)
            .ok_or_else(|| Error::custom(format!("unsupported Bybit side: {value}"))),
    }
}

pub(crate) fn deserialize_order_status<'de, D>(
    deserializer: D,
) -> Result<BybitOrderStatus, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;

    let value = String::deserialize(deserializer)?;
    parse_order_status(&value)
        .ok_or_else(|| Error::custom(format!("unsupported Bybit order status: {value}")))
}

fn parse_side(value: &str) -> Option<Side> {
    match value {
        "Buy" | "BUY" | "buy" => Some(Side::Buy),
        "Sell" | "SELL" | "sell" => Some(Side::Sell),
        _ => None,
    }
}

fn parse_order_status(value: &str) -> Option<BybitOrderStatus> {
    match value {
        "New" | "NEW" => Some(BybitOrderStatus::New),
        "PartiallyFilled" | "PARTIALLY_FILLED" => Some(BybitOrderStatus::PartiallyFilled),
        "Filled" | "FILLED" => Some(BybitOrderStatus::Filled),
        "Cancelled" | "CANCELED" => Some(BybitOrderStatus::Cancelled),
        "Rejected" | "REJECTED" => Some(BybitOrderStatus::Rejected),
        "Expired" | "EXPIRED" => Some(BybitOrderStatus::Expired),
        "Untriggered" | "UNTRIGGERED" => Some(BybitOrderStatus::Untriggered),
        "Triggered" | "TRIGGERED" => Some(BybitOrderStatus::Triggered),
        "Deactivated" | "DEACTIVATED" => Some(BybitOrderStatus::Deactivated),
        _ => None,
    }
}
