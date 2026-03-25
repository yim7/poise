# 协议契约

本文档描述当前 `grid-server` 与 `grid-tui` 之间实际生效的 HTTP / WebSocket 协议。Rust 类型定义仍以 `protocol/src/lib.rs` 为准；本文档只补充线协议语义和兼容规则。

## 1. 兼容规则

- 当前协议没有额外 envelope，HTTP 直接返回业务 DTO，WebSocket 直接返回事件 DTO。
- 字段新增只能追加，不能重命名或改变既有字段语义。
- 如果需要删除字段或修改字段语义，必须先引入新版本路由或新的 DTO。

## 2. HTTP 路由

当前服务端暴露 3 个业务路由：

- `GET /grids`
  返回 `Vec<GridSummary>`。
- `GET /grids/:id/snapshot`
  返回单个 `GridSnapshot`。
- `POST /grids/:id/commands`
  接收 `CommandRequest`，返回 `CommandResponse`。

当前 `CommandRequest.command` 只接受：

- `pause`
- `resume`

错误响应当前统一返回：

```json
{
  "error": "..."
}
```

其中：

- `400` 表示请求参数或命令语义错误。
- `404` 表示目标 `grid_id` 不存在。
- `500` 表示状态持久化或运行时内部错误。

## 3. HTTP DTO

`GET /grids` 返回的 `GridSummary` 字段：

- `id`
- `symbol`
- `status`
- `reference_price`

`GET /grids/:id/snapshot` 返回的 `GridSnapshot` 字段：

- `id`
- `symbol`
- `status`
- `current_exposure`
- `target_exposure`
- `reference_price`
- `pending_order`
- `config`

说明：

- `id` / `grid_id` 是控制面稳定身份，例如 `btc-core`。
- `symbol` 是交易所市场标识，例如 `BTCUSDT`。
- `reference_price` 是当前策略使用的参考价；当前 Binance 适配层把 mark price 作为参考价输入。
- `pending_order` 为空表示当前没有待跟踪委托。
- `config` 里的容量字段已经统一为 `long_exposure_units`、`short_exposure_units`、`notional_per_unit`。

## 4. WebSocket

当前只暴露一个 WebSocket 入口：

- `GET /ws`

服务端推送 `WsEvent`：

```json
{
  "grid_id": "btc-core",
  "event": {
    "band_reentered": {
      "price": 99.0
    }
  }
}
```

说明：

- `grid_id` 是当前一等业务标识，不等于 `symbol`。
- `event` 使用 `DomainEvent` 的 `snake_case` 序列化形式。
- WebSocket 事件只做增量通知；完整状态以 `GET /grids/:id/snapshot` 为准。

## 5. 客户端约定

- TUI 先拉 `GET /grids`，再拉当前选中网格的 `GET /grids/:id/snapshot`。
- WebSocket 断线重连后，客户端应重新拉一次当前快照，不应只依赖缓存事件回放。
- 客户端必须允许 `target_exposure`、`reference_price`、`pending_order` 为空。
