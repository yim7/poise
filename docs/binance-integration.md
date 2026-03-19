# Binance 接入说明

本文档记录当前 `K3` 已落地的 Binance 接入边界、运行方式和后续扩展落点。

## 1. 当前接入范围

当前 `service` 已接入四类 Binance 能力：

- `exchangeInfo`
- `tradingSchedule`
- 市场 WebSocket 流
- 用户流骨架与位置同步

接入代码位于：

- [`../service/src/integrations/binance.rs`](../service/src/integrations/binance.rs)
- [`../service/tests/binance_integration.rs`](../service/tests/binance_integration.rs)

## 2. 模块职责

`integrations/binance` 当前按下面的职责划分：

- REST：
  - 拉取 `exchangeInfo`
  - 拉取 `tradingSchedule`
  - 创建和保活用户流 `listenKey`
- 市场流：
  - 订阅成交与标记价
  - 推送 `last_price / mark_price`
  - 驱动 heartbeat 与 stale 年龄
- 用户流：
  - 维护用户流连接
  - 接收账户更新
  - 提取当前 `symbol` 的位置数据
- 连接健康：
  - 聚合 REST、市场流、用户流状态
  - 统一写回内核 `ConnectionState`

## 3. 内核写入路径

Binance 适配层不会直接修改共享状态，所有外部更新都通过 `EngineHandle` 进入单写者路径：

- 连接状态通过 `sync_connection`
- 行情通过 `sync_market_prices`
- session / 账户位置通过 `sync_runtime`

这样可以保证：

- REST、市场流、用户流都走统一状态入口
- TUI 看到的连接健康与运行态来自同一份权威状态
- fake transport 测试与真实 transport 行为保持一致

## 4. session_state 语义

当前 `session_state` 的来源如下：

- 对 `EQUITY` / `COMMODITY` 标的，来自 `tradingSchedule`
- 对持续交易标的，写为 `continuous`
- 若交易状态不是 `TRADING`，优先反映交易状态本身

## 5. 运行方式

默认本地运行仍是当前骨架模式。

要启用真实 Binance 接入，需要显式设置：

```bash
export GRID_PLATFORM_BINANCE_ENABLED=1
export GRID_PLATFORM_BINANCE_ENV=testnet
export GRID_PLATFORM_BINANCE_SYMBOL=XAUUSDT
export GRID_PLATFORM_BINANCE_API_KEY=your_api_key
```

然后运行：

```bash
cargo run -p grid-platform-service
```

说明：

- 未设置 `GRID_PLATFORM_BINANCE_ENABLED=1` 时，不会发起真实 Binance 连接
- 未配置 `GRID_PLATFORM_BINANCE_API_KEY` 时，用户流保持未配置状态，但市场元数据与市场流仍可工作

## 6. 当前测试覆盖

当前已覆盖的 K3 验收测试包括：

- fake transport 下的 `exchangeInfo + tradingSchedule` 同步
- 市场流价格进入 `service`
- reconnect / heartbeat / stale 检测
- 用户流位置更新进入运行态
- TUI 对真实连接状态与 session 状态的快照回归

本地统一验证命令：

```bash
cargo test
```
