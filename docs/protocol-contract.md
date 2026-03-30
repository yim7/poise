# Poise 协议契约

本文档描述 `Poise` 当前 `grid-server` 与 `grid-tui` 实际使用的 HTTP / WebSocket 协议。Rust 类型定义以 `protocol/src/lib.rs` 为准；本文档只说明当前线协议和接口语义。

为保持兼容，当前线协议中的稳定标识和代码术语仍沿用 `grid` / `grid_id` 命名。

## 1. HTTP 路由

当前服务端暴露 3 个业务路由：

- `GET /grids`
  返回 `GridListResponse`。
- `GET /grids/:id`
  返回单个 `GridDetailView`。
- `POST /grids/:id/commands`
  接收 `GridCommandRequest`，返回 `GridCommandAccepted`。

错误响应统一返回：

```json
{
  "error": "..."
}
```

状态码语义：

- `400`：请求参数错误、命令当前不可执行，或命令尚未实现。
- `404`：目标 `grid_id` 不存在。
- `500`：查询、持久化或运行时内部错误。

## 2. HTTP DTO

### 2.1 `GET /grids` -> `GridListResponse`

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
      "reference_price": 101.25,
      "exposure": {
        "current": 3.5,
        "target": 4.0
      },
      "execution": {
        "state": "open",
        "execution_status": "normal",
        "active_slot_count": 1
      }
    }
  ]
}
```

字段语义：

- `id`：网格稳定标识，例如 `btc-core`。
- `instrument`：交易市场身份，当前包含 `venue` 和 `symbol`。
- `lifecycle`：网格生命周期状态与最近更新时间。
- `reference_price`：当前策略参考价；当前 Binance 适配层使用 mark price 作为参考价输入。
- `exposure`：当前和目标敞口摘要。
- `execution.state`：执行面是否处于 `open / paused / closed`。
- `execution.execution_status`：执行是否需要人工关注。当前稳定值为 `normal` 和 `attention_required`。
- `execution.active_slot_count`：当前执行器中有多少个活跃槽位。它表达的是槽位工作集数量，不等于交易所原始 open orders 数量。

### 2.2 `GET /grids/:id` -> `GridDetailView`

`GridDetailView` 按块组织详情：

- `identity`
- `status`
- `strategy`
- `market`
- `position`
- `execution`
- `activity`
- `available_commands`

其中：

- `statistics` 提供稳定累计统计，当前包含 `total_pnl`、`realized_pnl`、`max_inventory_gap_abs`、`max_gap_age_ms` 和 `stats_started_at`。
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

### 2.3 `POST /grids/:id/commands`

请求体：

```json
{
  "command": "pause"
}
```

响应体：

```json
{
  "grid_id": "btc-core",
  "command": "pause",
  "accepted": true
}
```

`GridCommandType` 目前对外暴露：

- `pause`
- `resume`
- `terminate`
- `flatten`

当前服务端实际只实现：

- `pause`
- `resume`
- `flatten`

`terminate` 目前会返回 `400`，错误消息形如：

```json
{
  "error": "command `terminate` is not implemented"
}
```

## 3. WebSocket

当前只暴露一个 WebSocket 入口：

- `GET /ws`

服务端推送 `GridStreamEvent`：

```json
{
  "grid_id": "btc-core",
  "payload": {
    "type": "grid_detail_changed",
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

- `grid_list_item_changed`
- `grid_detail_changed`

说明：

- `grid_id` 是控制面稳定标识，不等于 `symbol`。
- WebSocket 只推投影后的读模型更新，不再推原始领域事件。
- 如果服务端在推送阶段发现通知流 lagged，或当前 grid 的读模型缺失 / 读取失败，会主动关闭 `/ws`，客户端应重连后重新拉 `GET /grids` 和当前 `GET /grids/:id` 做 resync。

## 4. 客户端约定

- `grid-tui` 启动时先拉 `GET /grids`，再拉当前选中网格的 `GET /grids/:id`。
- WebSocket 推送到达后，TUI 直接应用 `grid_list_item_changed` / `grid_detail_changed`，不再做旧快照兼容解析。
- 客户端必须允许这些字段为空：
  - 列表：`items[].reference_price`、`items[].exposure.target`
  - 详情：`status.reference_price`、`market.mark_price`、`market.index_price`、`position.target_exposure`、`statistics.stats_started_at`、`execution.replacement_gate`
  - 详情槽位订单：`execution.slots[].order`
  - 命令描述：`available_commands[].disabled_reason`
