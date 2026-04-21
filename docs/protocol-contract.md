# Poise 协议契约

本文档描述 `Poise` 当前 `poise-server` 与 `poise-tui` 实际使用的 HTTP / WebSocket 协议。Rust 类型定义以 `protocol/src/lib.rs` 为准；本文档只说明当前线协议和接口语义。

当前线协议统一使用 `track` / `track_id` 命名。

## 1. HTTP 路由

当前服务端暴露 4 个业务路由：

- `GET /health`
  返回服务健康摘要。
- `GET /tracks`
  返回 `TrackListResponse`。
- `GET /tracks/:id`
  返回单个 `TrackDetailView`。
- `POST /tracks/:id/commands`
  接收 `TrackCommandRequest`，返回 `TrackCommandAccepted`。

错误响应统一返回：

```json
{
  "error": "..."
}
```

状态码语义：

- `400`：请求参数错误、命令当前不可执行，或命令尚未实现。
- `404`：目标 `track_id` 不存在。
- `500`：查询、持久化或运行时内部错误。

## 2. HTTP DTO

### 2.0 `GET /health`

响应体：

```json
{
  "status": "ok",
  "track_count": 1,
  "attention_required_count": 0
}
```

状态码语义：

- `200`：当前全部轨道都没有 `attention_required`
- `503`：至少一个轨道出现 `stale market data` 或 `recovery anomaly`

字段语义：

- `status`：当前服务健康摘要，稳定值为 `ok` 和 `attention_required`
- `track_count`：当前已加载轨道数
- `attention_required_count`：当前需要人工关注的轨道数

### 2.1 `GET /tracks` -> `TrackListResponse`

```json
{
  "items": [
    {
      "id": "btc-core",
      "instrument": {
        "venue": "binance_futures",
        "symbol": "BTCUSDT"
      },
      "lifecycle": {
        "status": "active",
        "updated_at": "2026-03-26T10:00:00+00:00"
      },
      "strategy_price": 101.25,
      "strategy_price_status": "live",
      "exposure": {
        "current": 3.5,
        "target": 4.0
      },
      "execution": {
        "state": "open",
        "execution_status": "normal",
        "active_slot_count": 1
      },
      "ledger": {
        "total_pnl": 1245.3,
        "has_unresolved_gaps": false
      }
    }
  ]
}
```

字段语义：

- `id`：轨道稳定标识，例如 `btc-core`。
- `instrument`：交易市场身份，当前包含 `venue` 和 `symbol`。
- `lifecycle`：轨道生命周期状态与最近更新时间。
- `strategy_price`：当前策略价格，定义为盘口中间价 `book_mid = (best_bid + best_ask) / 2`。
- `strategy_price_status`：当前策略价格是否为 `live / stale`。
- `exposure`：当前和目标敞口摘要。
- `ledger.total_pnl`：列表视图只暴露累计总盈亏摘要，不携带详情页里的拆分口径。
- `ledger.has_unresolved_gaps`：当前累计账本里是否还有未解决 gap。
- `execution.state`：执行面是否处于 `open / paused / closed`。
- `execution.execution_status`：执行是否需要人工关注。当前稳定值为 `normal` 和 `attention_required`。
- `execution.active_slot_count`：当前执行器中有多少个活跃槽位。它表达的是槽位工作集数量，不等于交易所原始 open orders 数量。

### 2.2 `GET /tracks/:id` -> `TrackDetailView`

`TrackDetailView` 按块组织详情：

- `identity`
- `status`
- `strategy`
- `budget`
- `market`
- `position`
- `ledger`
- `execution_stats`
- `execution`
- `activity`
- `available_commands`

其中：

- `status` 提供生命周期和策略价格摘要，当前包含 `lifecycle`、`strategy_price` 和 `strategy_price_status`。
- `strategy` 提供配置后的价格带、仓位单位、形状族和 `out_of_band_policy`。
- `strategy.out_of_band_policy` 当前稳定形状是嵌套 policy object，例如 `{"freeze":{}}`、`{"flatten":{"trigger":{"flatten_confirm":{"bps":500}},"recover":{"reentry_confirm":{"bps":500}}}}`、`{"terminate":{}}`。
- `budget` 提供当前轨道风险预算。
- `market` 提供 `mark_price`、`best_bid` 和 `best_ask`。
- `ledger` 提供累计盈亏读模型，当前包含 `gross_realized_pnl`、`net_realized_pnl`、`unrealized_pnl`、`total_pnl`、费用累计和未解决 ledger gaps。
- `execution_stats` 提供执行统计窗口读模型，当前包含 `max_inventory_gap_abs`、`max_gap_age_ms` 和 `stats_started_at`。
- `execution` 提供执行摘要，当前包含：
  - `state`
  - `execution_status`
  - `inventory_gap`
  - `gap_age_ms`
  - `active_slot_count`
  - `slots`
  - `replacement_gate`
- `execution.slots` 是执行器对外稳定槽位视图。每个槽位只暴露：
  - `label`
  - `phase`
  - `intent`
  - `order`
- `execution.slots[].order` 只包含 `side`、`price`、`quantity`；不暴露 `client_order_id`、交易所订单状态或内部恢复字段。
- `activity` 是已经投影过的活动流，不直接暴露原始 `DomainEvent`。
- `available_commands` 直接给出命令是否可执行以及禁用原因，客户端不再自行推断。

### 2.3 `POST /tracks/:id/commands`

请求体：

```json
{
  "command": "pause"
}
```

响应体：

```json
{
  "track_id": "btc-core",
  "command": "pause",
  "accepted": true
}
```

`TrackCommandType` 目前对外暴露：

- `pause`
- `resume`
- `terminate`
- `flatten`

当前服务端实际只实现：

- `pause`
- `resume`
- `terminate`
- `flatten`

`resume` 的语义：

- 在 `paused`、`holding` 或 `manual_flattening` 时可执行
- 从 `holding` 或 `manual_flattening` 恢复正常策略控制
- 如果当前没有 live `strategy_price`，恢复到 `waiting_market_data`

`flatten` 的语义：

- 写入人工目标覆盖 `manual_target_override = 0`
- 生命周期进入 `manual_flattening`
- 回带内后不会自动恢复，必须执行 `resume`

`terminate` 的语义：

- 进入真正终态
- 目标占用收敛到 `0`
- 后续不会因为价格重新进入带内而恢复策略目标
- 已终止轨道不能再执行 `pause`、`resume`、`terminate` 或 `flatten`

### 2.4 `GET /debug/tracks/:id/diagnostics` -> `TrackDiagnosticsView`

这个接口属于 debug 命名空间，不是稳定产品协议的一部分。

响应体：

```json
{
  "items": [
    {
      "ts": "2026-04-03T02:26:47Z",
      "message": "desired exposure -3.9534 -> -3.7500",
      "level": "info"
    }
  ]
}
```

字段语义：

- `items`：按时间排序的 diagnostics 时间线
- `ts`：事件时间戳，使用 RFC3339 UTC 字符串
- `message`：供排查使用的诊断文案
- `level`：诊断级别，当前稳定值为 `info`、`warn`、`error`

说明：

- `GET /tracks/:id` 继续返回稳定用户详情，不包含 diagnostics
- `/debug/...` 下的 diagnostics 为 debug 专用、非稳定、best-effort 接口
- diagnostics 不作为自动化、告警或外部集成契约
- 当前 TUI 默认不请求这个接口，只有进入显式 debug 视角时才按需加载

## 3. WebSocket

当前只暴露一个 WebSocket 入口：

- `GET /ws`

服务端推送 `TrackStreamEvent`：

```json
{
  "track_id": "btc-core",
  "payload": {
    "type": "track_detail_changed",
    "detail": {
      "identity": {
        "id": "btc-core",
        "instrument": {
          "venue": "binance_futures",
          "symbol": "BTCUSDT"
        }
      }
    }
  }
}
```

`payload` 目前只有两种变体：

- `track_list_item_changed`
- `track_detail_changed`

说明：

- `track_id` 是控制面稳定标识，不等于 `symbol`。
- WebSocket 只推投影后的读模型更新，不再推原始领域事件。
- 如果服务端在推送阶段发现通知流 lagged，或当前轨道的读模型缺失 / 读取失败，会主动关闭 `/ws`，客户端应重连后重新拉 `GET /tracks` 和当前 `GET /tracks/:id` 做 resync。

## 4. 客户端约定

- `poise-tui` 启动时先拉 `GET /tracks`，再拉当前选中轨道的 `GET /tracks/:id`。
- WebSocket 推送到达后，TUI 直接应用 `track_list_item_changed` / `track_detail_changed`，不再做旧快照兼容解析。
- 客户端必须允许这些字段为空：
  - 列表：`items[].reference_price`、`items[].exposure.target`
  - 详情：`status.reference_price`、`market.mark_price`、`market.index_price`、`position.desired_exposure`、`execution_stats.stats_started_at`、`execution.replacement_gate`
  - 详情槽位订单：`execution.slots[].order`
  - 命令描述：`available_commands[].disabled_reason`
