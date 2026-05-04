use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OkxEnvelope<T> {
    pub code: String,
    pub msg: String,
    pub data: Vec<T>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InstrumentInfo {
    #[serde(rename = "instId")]
    pub inst_id: String,
    #[serde(rename = "tickSz")]
    pub tick_sz: String,
    #[serde(rename = "lotSz")]
    pub lot_sz: String,
    #[serde(rename = "minSz")]
    pub min_sz: String,
    #[serde(rename = "ctVal")]
    pub ct_val: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BalanceSnapshot {
    #[serde(rename = "totalEq")]
    pub total_eq: String,
    #[serde(default)]
    pub details: Vec<BalanceDetail>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct BalanceDetail {
    #[serde(rename = "ccy")]
    pub currency: String,
    #[serde(rename = "availEq")]
    pub avail_eq: String,
    #[serde(rename = "upl")]
    pub upl: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PositionSnapshot {
    #[serde(rename = "instId")]
    pub inst_id: String,
    pub pos: String,
    #[serde(rename = "avgPx")]
    pub avg_px: String,
    pub upl: String,
    #[serde(rename = "posSide")]
    pub pos_side: String,
    pub lever: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PendingOrderSnapshot {
    #[serde(rename = "instId")]
    pub inst_id: String,
    #[serde(rename = "ordId")]
    pub order_id: String,
    #[serde(rename = "clOrdId")]
    pub client_order_id: String,
    pub side: String,
    #[serde(rename = "px")]
    pub price: String,
    #[serde(rename = "sz")]
    pub size: String,
    #[serde(rename = "accFillSz")]
    pub acc_fill_sz: String,
    pub state: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OrderAck {
    #[serde(rename = "ordId")]
    pub order_id: String,
    #[serde(rename = "clOrdId")]
    pub client_order_id: String,
    #[serde(rename = "sCode")]
    pub s_code: String,
    #[serde(rename = "sMsg")]
    pub s_msg: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ServerTime {
    pub ts: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_instrument_envelope_with_okx_field_names() {
        let payload = r#"
        {
            "code": "0",
            "msg": "",
            "data": [
                {
                    "instId": "BTC-USDT-SWAP",
                    "tickSz": "0.1",
                    "lotSz": "0.01",
                    "minSz": "0.01",
                    "ctVal": "0.01"
                }
            ]
        }
        "#;

        let envelope: OkxEnvelope<InstrumentInfo> = serde_json::from_str(payload).unwrap();

        assert_eq!(envelope.code, "0");
        assert_eq!(envelope.msg, "");
        assert_eq!(envelope.data[0].inst_id, "BTC-USDT-SWAP");
        assert_eq!(envelope.data[0].tick_sz, "0.1");
        assert_eq!(envelope.data[0].lot_sz, "0.01");
        assert_eq!(envelope.data[0].min_sz, "0.01");
        assert_eq!(envelope.data[0].ct_val.as_deref(), Some("0.01"));
    }

    #[test]
    fn deserializes_account_position_order_ack_and_time_models() {
        let balance: BalanceSnapshot = serde_json::from_str(
            r#"
            {
                "totalEq": "12500.5",
                "details": [
                    { "ccy": "USDT", "availEq": "9800.25", "upl": "-120.75" }
                ]
            }
            "#,
        )
        .unwrap();
        assert_eq!(balance.total_eq, "12500.5");
        assert_eq!(balance.details[0].currency, "USDT");
        assert_eq!(balance.details[0].avail_eq, "9800.25");
        assert_eq!(balance.details[0].upl, "-120.75");

        let position: PositionSnapshot = serde_json::from_str(
            r#"
            {
                "instId": "BTC-USDT-SWAP",
                "pos": "-0.25",
                "avgPx": "65000.5",
                "upl": "123.45",
                "posSide": "net",
                "lever": "20"
            }
            "#,
        )
        .unwrap();
        assert_eq!(position.inst_id, "BTC-USDT-SWAP");
        assert_eq!(position.pos, "-0.25");
        assert_eq!(position.avg_px, "65000.5");
        assert_eq!(position.pos_side, "net");

        let order: PendingOrderSnapshot = serde_json::from_str(
            r#"
            {
                "instId": "BTC-USDT-SWAP",
                "ordId": "123",
                "clOrdId": "client-123",
                "side": "buy",
                "px": "65000.1",
                "sz": "0.2",
                "accFillSz": "0.05",
                "state": "partially_filled"
            }
            "#,
        )
        .unwrap();
        assert_eq!(order.order_id, "123");
        assert_eq!(order.client_order_id, "client-123");
        assert_eq!(order.acc_fill_sz, "0.05");

        let ack: OrderAck = serde_json::from_str(
            r#"{ "ordId": "123", "clOrdId": "client-123", "sCode": "0", "sMsg": "" }"#,
        )
        .unwrap();
        assert_eq!(ack.order_id, "123");
        assert_eq!(ack.client_order_id, "client-123");
        assert_eq!(ack.s_code, "0");
        assert_eq!(ack.s_msg, "");

        let time: ServerTime = serde_json::from_str(r#"{ "ts": "1704876947123" }"#).unwrap();
        assert_eq!(time.ts, "1704876947123");
    }
}
