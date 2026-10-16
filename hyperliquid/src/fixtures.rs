//! 交易所响应的脱敏样本。
//!
//! 内容是 `.scratch/layered-grid/fixtures/hyperliquid/` 那份样本的副本（地址、订单号
//! 都已经换成占位值）。放在 crate 里是为了让适配测试不依赖仓库里的临时目录，
//! 也不依赖网络。

/// `{"type":"userAbstraction"}`。
pub(crate) const USER_ABSTRACTION: &str = r#""unifiedAccount""#;

/// `{"type":"extraAgents"}`。
pub(crate) const EXTRA_AGENTS: &str = r#"[
  {
    "name": "poise",
    "address": "0x0000000000000000000000000000000000000000",
    "validUntil": 1800000000000
  }
]"#;

/// `{"type":"spotClearinghouseState"}`。
pub(crate) const SPOT_CLEARINGHOUSE_STATE: &str = r#"{
  "balances": [
    {
      "coin": "USDC",
      "token": 0,
      "total": "1000.0",
      "hold": "120.5",
      "entryNtl": "0.0"
    }
  ],
  "tokenToAvailableAfterMaintenance": [
    [
      0,
      "870.25"
    ]
  ]
}"#;

/// `{"type":"userFees"}`。
pub(crate) const USER_FEES: &str = r#"{
  "userCrossRate": "0.00045",
  "userAddRate": "0.00015"
}"#;

/// `{"type":"clearinghouseState"}`。
pub(crate) const CLEARINGHOUSE_STATE: &str = r#"{
  "marginSummary": {
    "accountValue": "1000.0",
    "totalNtlPos": "500.0",
    "totalRawUsd": "500.0",
    "totalMarginUsed": "50.0"
  },
  "crossMarginSummary": {
    "accountValue": "1000.0",
    "totalNtlPos": "500.0",
    "totalRawUsd": "500.0",
    "totalMarginUsed": "50.0"
  },
  "crossMaintenanceMarginUsed": "12.5",
  "withdrawable": "0.0",
  "assetPositions": [
    {
      "type": "oneWay",
      "position": {
        "coin": "BTC",
        "szi": "0.005",
        "leverage": {
          "type": "cross",
          "value": 10
        },
        "entryPx": "100000.0",
        "positionValue": "500.0",
        "unrealizedPnl": "0.0",
        "returnOnEquity": "0.0"
      }
    }
  ],
  "time": 1792000000000
}"#;

/// `{"type":"openOrders"}`。
pub(crate) const OPEN_ORDERS: &str = r#"[
  {
    "coin": "BTC",
    "side": "B",
    "limitPx": "94000.0",
    "sz": "0.001",
    "oid": 1004,
    "timestamp": 1792000000000,
    "origSz": "0.001"
  }
]"#;

/// 我们自己下的挂单：多了 `cloid`，`sz` 是部分成交后的剩余量。
pub(crate) const OPEN_ORDERS_OURS: &str = r#"[
  {
    "coin": "BTC",
    "side": "B",
    "limitPx": "94000.0",
    "sz": "0.0006",
    "oid": 1004,
    "timestamp": 1792000000000,
    "origSz": "0.001",
    "cloid": "0x11111111111111111111111111111111"
  }
]"#;

/// `{"type":"orderStatus"}`：交易所不认识这个订单 ID。
pub(crate) const ORDER_STATUS_UNKNOWN: &str = r#"{
  "status": "unknownOid"
}"#;

/// `{"type":"orderStatus"}`：还挂着的单。
pub(crate) const ORDER_STATUS_OPEN: &str = r#"{
  "status": "order",
  "order": {
    "order": {
      "coin": "BTC",
      "side": "B",
      "limitPx": "94000.0",
      "sz": "0.001",
      "oid": 1004,
      "timestamp": 1792000000000,
      "origSz": "0.001",
      "cloid": "0x11111111111111111111111111111111"
    },
    "status": "open",
    "statusTimestamp": 1792000000000
  }
}"#;

/// `{"type":"orderStatus"}`：已经撤销的单。
pub(crate) const ORDER_STATUS_CANCELED: &str = r#"{
  "status": "order",
  "order": {
    "order": {
      "coin": "BTC",
      "side": "B",
      "limitPx": "94000.0",
      "sz": "0.001",
      "oid": 1004,
      "timestamp": 1792000000000,
      "origSz": "0.001",
      "cloid": "0x11111111111111111111111111111111"
    },
    "status": "canceled",
    "statusTimestamp": 1792000060000
  }
}"#;

/// `{"type":"userFillsByTime"}`：两笔买入、一笔卖出，最后一笔没有 `cloid`。
pub(crate) const USER_FILLS_BY_TIME: &str = r#"[
  {
    "coin": "BTC",
    "px": "95000.0",
    "sz": "0.001",
    "side": "B",
    "time": 1792000000000,
    "startPosition": "0.0",
    "dir": "Open Long",
    "closedPnl": "0.0",
    "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "oid": 1001,
    "crossed": false,
    "fee": "0.014",
    "tid": 5001,
    "cloid": "0x11111111111111111111111111111111",
    "feeToken": "USDC",
    "twapId": null
  },
  {
    "coin": "BTC",
    "px": "95000.0",
    "sz": "0.002",
    "side": "B",
    "time": 1792000000000,
    "startPosition": "0.001",
    "dir": "Open Long",
    "closedPnl": "0.0",
    "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "oid": 1002,
    "crossed": false,
    "fee": "0.028",
    "tid": 5002,
    "cloid": "0x22222222222222222222222222222222",
    "feeToken": "USDC",
    "twapId": null
  },
  {
    "coin": "BTC",
    "px": "96000.0",
    "sz": "0.001",
    "side": "A",
    "time": 1792000060000,
    "startPosition": "0.003",
    "dir": "Close Long",
    "closedPnl": "1.0",
    "hash": "0x0000000000000000000000000000000000000000000000000000000000000000",
    "oid": 1003,
    "crossed": true,
    "fee": "0.043",
    "tid": 5003,
    "feeToken": "USDC",
    "twapId": null
  }
]"#;

/// `{"type":"meta"}`。
pub(crate) const META: &str = r#"{
  "universe": [
    {"name": "ETH", "szDecimals": 4, "maxLeverage": 25},
    {"name": "BTC", "szDecimals": 5, "maxLeverage": 40}
  ]
}"#;

/// `{"type":"metaAndAssetCtxs"}`。
pub(crate) const META_AND_ASSET_CTXS: &str = r#"[
  {"universe": [{"name": "ETH", "szDecimals": 4}, {"name": "BTC", "szDecimals": 5}]},
  [{"markPx": "2500.5", "funding": "0.0000125"}, {"markPx": "95000.0", "funding": "-0.0002110251"}]
]"#;

/// 下单成功：挂在盘口上。
pub(crate) const ORDER_RESTING: &str = r#"{
  "status": "ok",
  "response": {"type": "order", "data": {"statuses": [{"resting": {"oid": 1004}}]}}
}"#;

/// 下单成功：立刻成交。
pub(crate) const ORDER_FILLED: &str = r#"{
  "status": "ok",
  "response": {"type": "order", "data": {"statuses": [{"filled": {"oid": 1005, "totalSz": "0.001", "avgPx": "95000.0"}}]}}
}"#;

/// 下单因为按地址的累计请求上限被拒（原话格式来自社区报告，文档没有给出）。
pub(crate) const ORDER_RATE_LIMITED: &str = r#"{
  "status": "ok",
  "response": {"type": "order", "data": {"statuses": [{"error": "Too many cumulative requests sent (10001 > 10000) for cumulative volume traded $0.00. Place taker orders to free up 1 request per USDC traded."}]}}
}"#;

/// 下单被拒。
pub(crate) const ORDER_REJECTED: &str = r#"{
  "status": "ok",
  "response": {"type": "order", "data": {"statuses": [{"error": "Insufficient margin to place order."}]}}
}"#;

/// 整个动作被拒。
pub(crate) const ACTION_REJECTED: &str = r#"{
  "status": "err",
  "response": "Must deposit before performing actions. User: 0x0000000000000000000000000000000000000000"
}"#;

/// 撤单成功。
pub(crate) const CANCEL_OK: &str = r#"{
  "status": "ok",
  "response": {"type": "cancel", "data": {"statuses": ["success"]}}
}"#;

/// 撤单时交易所说这张单已经撤过或已成交。
///
/// 这是测试网上撤掉一张已经不在场的单时交易所的原话（`asset` 后面的数字是标的序号，
/// 这里按本文件的假合约写成 `0`）。
pub(crate) const CANCEL_ALREADY_DONE: &str = r#"{
  "status": "ok",
  "response": {"type": "cancel", "data": {"statuses": [{"error": "Order was never placed, already canceled, or filled. asset=0"}]}}
}"#;

/// 同一句意思的另一种说法。交易所的措辞变过，两种都得认。
pub(crate) const CANCEL_ALREADY_DONE_OLD_WORDING: &str = r#"{
  "status": "ok",
  "response": {"type": "cancel", "data": {"statuses": [{"error": "Order has already been canceled or filled."}]}}
}"#;
