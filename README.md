# 网格平台

`grid-platform` 是一个面向 Binance USDⓈ-M Futures 的网格交易平台底座。当前仓库聚焦两件事：

- 把 `service` 做成唯一的领域状态中心和控制面宿主
- 把 `tui` 做成本地运维、验证和联调入口

两者通过统一的 HTTP / WebSocket 协议协作，SQLite 用于可选的持久化、恢复和审计。当前代码已经覆盖本地内存模式、SQLite 恢复链路，以及 testnet / replay / paper 的验证基线。

## 仓库结构

- [`service/`](service/)
  Rust 服务端。负责运行时状态、控制面接口、Binance 接入、审计与恢复。
- [`tui/`](tui/)
  Rust 终端客户端。负责监控、运维操作和本地链路验证。
- [`docs/`](docs/)
  架构、协议、Binance 接入、验证手册、路线图等项目文档。

## 快速启动

### 依赖

- 已安装 Rust 与 `cargo`
- 本地终端支持 TUI alternate screen

### 1. 启动服务端

```bash
cargo run -p grid-platform-service
```

默认行为：

- 监听 `127.0.0.1:8000`
- 使用 SQLite 路径 `.data/paper/local.db`
- 不连接真实 Binance

如果要按配置文件一次启动多个固定区间网格实例，先按下面示例创建 `configs/testnet.toml`，再传入 `--config`：

```bash
cargo run -p grid-platform-service -- --config configs/testnet.toml
```

配置文件示例：

```toml
environment = "testnet"
default_symbol = "BTCUSDT"

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 90000
upper_price = 110000
grid_levels = 6
max_position_notional = 3000

[[instances]]
symbol = "ETHUSDT"

[instances.range]
lower_price = 2200
upper_price = 2800
grid_levels = 8
max_position_notional = 4000
```

配置文件模式下：

- `service` 会读取 `[[instances]]` 中的所有标的
- `default_symbol` 决定旧单实例兼容路由和 TUI 默认选中的标的
- 每个实例的 SQLite 会自动落到 `.data/<environment>/<symbol-lowercase>.db`
- 网格使用固定区间梯子模型：价格在区间内才挂单，区间外空仓时进入等待态

### 2. 另开终端启动 TUI

```bash
cargo run -p grid-platform-tui
```

默认会连接：

- HTTP：`http://127.0.0.1:8000`
- WebSocket：`ws://127.0.0.1:8000/ws`

TUI 启动后会先拉取 `/instances`，选择 `default_symbol`，再连接该标的的实例作用域快照和 WebSocket。运行中可以在实例列表里切换当前查看的标的，其余详情页保持单实例视角。快捷键：

- `[`：切到上一个实例
- `]`：切到下一个实例

### 3. 快速确认服务可用

如果只想先确认服务端已经起来，可以先看实例目录：

```bash
curl http://127.0.0.1:8000/instances
```

再按具体标的查看快照：

```bash
curl http://127.0.0.1:8000/instances/BTCUSDT/runtime/snapshot
```

如果想走完整链路，启动 `tui` 后应能先拉到实例目录，再拉当前实例快照，并继续接收实例作用域 WebSocket 增量事件。

## 常用运行方式

### 使用 SQLite 持久化本地状态

```bash
export GRID_PLATFORM_SERVICE_DB_PATH=.tmp/grid-platform.db
cargo run -p grid-platform-service
```

未显式指定 `GRID_PLATFORM_SERVICE_DB_PATH` 时，服务端会按运行环境和实例名推导默认路径：

- `paper`：`.data/paper/<instance_id>.db`
- `testnet`：`.data/testnet/<instance_id>.db`
- `mainnet`：`.data/mainnet/<instance_id>.db`

默认 `instance_id` 为 `local`，可通过 `GRID_PLATFORM_INSTANCE_ID` 覆盖。

### 接 Binance testnet

```bash
export GRID_PLATFORM_BINANCE_ENABLED=1
export GRID_PLATFORM_BINANCE_ENV=testnet
export GRID_PLATFORM_BINANCE_SYMBOL=XAUUSDT
export GRID_PLATFORM_BINANCE_API_KEY=your_api_key
export GRID_PLATFORM_SERVICE_DB_PATH=.tmp/testnet.db
cargo run -p grid-platform-service
```

补充说明：

- `GRID_PLATFORM_BINANCE_API_KEY` 当前不是强制项
- 未配置 API Key 时，用户流不会建立，但市场元数据与市场流仍可用于联调
- `GRID_PLATFORM_BINANCE_ENV=mainnet` 时，必须额外设置 `GRID_PLATFORM_ALLOW_MAINNET=1`
- mainnet 启动前必须收集签名持仓快照和签名挂单快照，因此需要同时配置 `GRID_PLATFORM_BINANCE_API_KEY` 与 `GRID_PLATFORM_BINANCE_API_SECRET`
- mainnet 启动前会执行启动对账；发现交易所持仓或交易所挂单与本地持久化状态明显不一致时，会先进入暂停态而不是继续自动下单

### 自定义服务端与 TUI 连接地址

服务端：

```bash
export GRID_PLATFORM_SERVICE_ADDR=127.0.0.1:9000
cargo run -p grid-platform-service
```

TUI：

```bash
export GRID_PLATFORM_BASE_URL=http://127.0.0.1:9000
export GRID_PLATFORM_WS_URL=ws://127.0.0.1:9000/ws
cargo run -p grid-platform-tui
```

## 开发与验证

运行全部测试：

```bash
cargo test
```

如果只想先验证服务端控制面：

```bash
cargo test -p grid-platform-service --test control_plane -- --nocapture
```

## 文档入口

- [`docs/technical-architecture.md`](docs/technical-architecture.md)：系统边界、职责划分与运行时模型
- [`docs/protocol-contract.md`](docs/protocol-contract.md)：`service` 与 `tui` 当前共享的 HTTP / WebSocket 协议
- [`docs/binance-integration.md`](docs/binance-integration.md)：真实 Binance 接入说明
- [`docs/k6-validation.md`](docs/k6-validation.md)：replay / paper / testnet 验证手册
- [`docs/roadmap.md`](docs/roadmap.md)：阶段目标与里程碑
- [`docs/plan.md`](docs/plan.md)：近期计划与验收标准
- [`TODO.md`](TODO.md)：当前任务清单与最近验证结果

## 项目说明

- 当前仓库是一个 Rust workspace，包含 `service` 和 `tui` 两个 crate
- `service` 是唯一的服务端状态中心，未来 Web UI 也会复用同一套控制面
- `tui` 通过 `instances + snapshot + incremental events` 模型消费服务端能力
- 对外协议约束同时以 Rust 类型定义和线协议语义文档为准
