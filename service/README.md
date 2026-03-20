# 网格平台服务端

这是网格平台的 Rust 服务端子项目。

当前定位：

- 作为网格平台的核心控制面
- 向 `tui` 和未来 Web UI 暴露 HTTP / WebSocket 接口
- 后续承载持久化、Binance 接入、网格策略和风控引擎

当前结构保持单 crate，内部按模块组织，避免过早拆分：

- `protocol`
- `control_plane`
- `application`
- `kernel`
- `storage`
- `integrations`
- 后续会继续补 `strategy`

## 运行

```bash
cargo run -p grid-platform-service
```

启用真实 Binance 接入时，可额外设置：

- `GRID_PLATFORM_BINANCE_ENABLED=1`
- `GRID_PLATFORM_BINANCE_ENV=testnet` 或 `mainnet`
- `GRID_PLATFORM_BINANCE_SYMBOL=XAUUSDT`
- `GRID_PLATFORM_BINANCE_API_KEY=...`

可选覆盖默认地址：

- `GRID_PLATFORM_BINANCE_REST_BASE_URL`
- `GRID_PLATFORM_BINANCE_WS_BASE_URL`
- `GRID_PLATFORM_INSTANCE_ID`，默认值为 `local`

## Web 查询接口

当前控制面同时保留 TUI 兼容接口和 Web 查询接口：

- TUI 兼容接口：`/runtime/snapshot`、`/orders/open`、`/fills/recent`、`/risk/events`、`/system/events`
- Web 查询接口：`/query/runtime`、`/query/orders`、`/query/fills`、`/query/alerts`、`/query/commands`
- 能力接口：`/control-plane/capabilities`

列表查询统一返回：

- `items`
- `pagination`
- `filters`
- `sort`

当前约定：

- `commands` 默认按 `requested_at_desc` 排序
- `alerts` 默认按 `created_at_desc` 排序
- WebSocket 当前只暴露 `runtime_stream` 单一订阅模型
- 简单认证边界采用 `Authorization` header 或 `access_token` query 参数预留

## 测试

```bash
cargo test -p grid-platform-service
```
