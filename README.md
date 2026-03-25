# 网格平台

`grid-platform` 是一个面向 Binance USDⓈ-M Futures 的探索型网格交易项目。当前主线实现已经统一到下面这套结构：

- `grid-server` 负责运行态、控制面、持久化和交易所接入
- `grid-tui` 负责本地值守、联调和操作入口
- `grid-protocol` 负责 `server` 与 `tui` 共享 DTO

项目仍在持续调整设计，旧方案不会保留兼容层。文档以本文件、[`docs/protocol-contract.md`](docs/protocol-contract.md) 和当前架构 spec / plan 为准。

## 工作区结构

- [`core/`](core/)：纯领域模型、策略参数、风险规则、领域事件
- [`engine/`](engine/)：单网格状态机、注册表、对账逻辑
- [`storage/`](storage/)：SQLite 快照与领域事件存储
- [`protocol/`](protocol/)：对外 HTTP / WebSocket DTO
- [`exchanges/binance/`](exchanges/binance/)：Binance REST / WebSocket 适配
- [`server/`](server/)：服务端装配、应用服务、HTTP / WS 入口
- [`tui/`](tui/)：终端运维界面

## 当前约束

- 同一交易所内，同一 `symbol` 只允许一个网格
- `grid_id` 是显式配置的稳定标识，不由 `symbol` 派生
- HTTP / WebSocket 以 `grid_id` 作为一等标识
- SQLite 默认路径是 `.data/<environment>/grid-server.sqlite`
- Binance 适配层当前用 `mark price` 作为策略 `reference_price`

## 快速开始

### 1. 准备配置

服务端只接受 `--config <path>` 方式启动。可以直接参考 [`configs/test.toml`](configs/test.toml)，最小示例如下：

```toml
environment = "paper"
bind_address = "127.0.0.1:8000"

[exchange]
rest_base_url = "https://testnet.binancefuture.com"
ws_base_url = "wss://stream.binancefuture.com"

[[grids]]
grid_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 90000.0
upper_price = 110000.0
long_exposure_units = 8.0
short_exposure_units = 8.0
notional_per_unit = 375.0
```

补充说明：

- 可以继续追加 `[[grids]]`，每个网格都要配置唯一的 `grid_id`
- 当前同一交易所内每个 `symbol` 只能出现一次
- `environment` 只决定数据目录和环境名，不自动切换交易所地址
- 需要用户流时，在 `[exchange]` 里补 `api_key` 和 `api_secret`

### 2. 启动服务端

```bash
cargo run -p grid-server -- --config configs/test.toml
```

服务端启动后会：

- 读取配置中的全部网格
- 初始化 SQLite
- 建立 HTTP / WebSocket 控制面
- 在配置了 Binance 地址时接入市场数据

### 3. 启动 TUI

```bash
cargo run -p grid-tui
```

默认连接：

- HTTP：`http://127.0.0.1:8000`
- WebSocket：`ws://127.0.0.1:8000/ws`

如果要改地址，可以在启动前设置：

```bash
export GRID_PLATFORM_BASE_URL=http://127.0.0.1:9000
export GRID_PLATFORM_WS_URL=ws://127.0.0.1:9000/ws
cargo run -p grid-tui
```

`grid-tui` 会先请求 `/grids`，再加载当前网格快照，并订阅 `/ws`。

### 4. 用 HTTP 快速确认

```bash
curl http://127.0.0.1:8000/grids
curl http://127.0.0.1:8000/grids/btc-core/snapshot
```

## 当前协议

当前对外接口只有 4 个入口：

- `GET /grids`
- `GET /grids/:id/snapshot`
- `POST /grids/:id/commands`
- `GET /ws`

字段和错误语义见 [`docs/protocol-contract.md`](docs/protocol-contract.md)。

## 数据

- 服务端按 `environment` 使用单个 SQLite 文件保存全部网格状态

## 开发与验证

常用命令：

```bash
cargo test -p grid-storage
cargo test -p grid-server
cargo test -p grid-tui
cargo test
```

最近一次完整验证已通过 `cargo test`。

## 当前文档

- [`docs/protocol-contract.md`](docs/protocol-contract.md)：当前 HTTP / WebSocket 协议
- [`docs/grid-strategy-product-theory-research.md`](docs/grid-strategy-product-theory-research.md)：当前策略研究与产品侧约束
- [`docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md`](docs/superpowers/specs/2026-03-24-grid-platform-architecture-design.md)：当前架构 spec
- [`docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md`](docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md)：当前策略族模型设计
- [`docs/superpowers/plans/2026-03-25-grid-platform-architecture-convergence.md`](docs/superpowers/plans/2026-03-25-grid-platform-architecture-convergence.md)：当前收敛计划与验收标准
