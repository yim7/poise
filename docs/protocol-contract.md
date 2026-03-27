# 协议契约

本文档描述当前 `grid-server` 与 `grid-tui` 实际使用的 HTTP / WebSocket 协议。Rust 类型定义以 `protocol/src/lib.rs` 为准；本文档只说明当前线协议和接口语义。

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
        "pending_order_count": 1
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
- `execution`：列表页执行摘要，只表达执行状态和待处理委托数量，不暴露原始委托对象。当前运行时只跟踪单个待处理执行，因此 `pending_order_count` 目前只会是 `0` 或 `1`。

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

- `execution.pending_order` 为空表示当前没有待处理委托。
- `execution.pending_order` 当前只包含 `symbol`、`order_id`、`side`、`price`、`quantity`、`status`，不暴露内部跟踪字段。
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

`terminate` 和 `flatten` 目前会返回 `400`，错误消息形如：

```json
{
  "error": "command `flatten` is not implemented"
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
  - 详情：`status.reference_price`、`market.mark_price`、`market.index_price`、`position.target_exposure`、`execution.pending_order`
  - 详情待处理委托：`execution.pending_order.order_id`
  - 命令描述：`available_commands[].disabled_reason`
