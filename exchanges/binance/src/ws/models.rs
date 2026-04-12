use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(super) enum MarketStreamEnvelope {
    Combined { data: MarketEvent },
    Plain(MarketEvent),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "e")]
pub(super) enum MarketEvent {
    #[serde(rename = "markPriceUpdate")]
    MarkPrice(MarkPriceMessage),
    #[serde(rename = "bookTicker")]
    BookTicker(BookTickerMessage),
}

#[derive(Debug, Deserialize)]
pub(super) struct MarkPriceMessage {
    #[serde(rename = "E")]
    pub(super) event_time: i64,
    #[serde(rename = "s")]
    pub(super) symbol: String,
    #[serde(rename = "p")]
    pub(super) mark_price: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct BookTickerMessage {
    #[serde(rename = "E")]
    pub(super) event_time: i64,
    #[serde(rename = "s")]
    pub(super) symbol: String,
    #[serde(rename = "b")]
    pub(super) best_bid: Option<String>,
    #[serde(rename = "a")]
    pub(super) best_ask: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct UserEventEnvelope {
    #[serde(rename = "e")]
    pub(super) event_type: String,
    #[serde(rename = "E")]
    pub(super) event_time: i64,
    #[serde(rename = "o")]
    pub(super) order: Option<OrderTradeUpdate>,
    #[serde(rename = "a")]
    pub(super) account: Option<AccountUpdate>,
}

#[derive(Debug, Deserialize)]
pub(super) struct OrderTradeUpdate {
    #[serde(rename = "s")]
    pub(super) symbol: String,
    #[serde(rename = "i")]
    pub(super) order_id: u64,
    #[serde(rename = "c")]
    pub(super) client_order_id: String,
    #[serde(rename = "S")]
    pub(super) side: String,
    #[serde(rename = "p")]
    pub(super) price: String,
    #[serde(rename = "q")]
    pub(super) quantity: String,
    #[serde(rename = "rp")]
    pub(super) realized_pnl: String,
    #[serde(rename = "n")]
    pub(super) commission_amount: Option<String>,
    #[serde(rename = "N")]
    pub(super) commission_asset: Option<String>,
    #[serde(rename = "X")]
    pub(super) status: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AccountUpdate {
    #[serde(rename = "m")]
    pub(super) reason: Option<String>,
    #[serde(rename = "B", default)]
    pub(super) balances: Vec<AccountBalanceUpdate>,
    #[serde(rename = "P")]
    pub(super) positions: Vec<AccountPositionUpdate>,
}

#[derive(Debug, Deserialize)]
pub(super) struct AccountBalanceUpdate {
    #[serde(rename = "a")]
    pub(super) asset: String,
    #[serde(rename = "bc")]
    pub(super) balance_change: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct AccountPositionUpdate {
    #[serde(rename = "s")]
    pub(super) symbol: String,
    #[serde(rename = "pa")]
    pub(super) position_amt: String,
    #[serde(rename = "ep")]
    pub(super) entry_price: String,
    #[serde(rename = "up")]
    pub(super) unrealized_profit: String,
}
