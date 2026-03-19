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

## 测试

```bash
cargo test -p grid-platform-service
```
