# Poise

`Poise` 是一个面向 Binance USDⓈ-M Futures 的探索型策略运行项目。当前主线把策略定义为价格带内的目标占用函数，并通过库存执行器持续把实际仓位拉回目标仓位。

当前主线实现已经统一到下面这套结构：

- `poise-server` 负责运行态、控制面、持久化和交易所接入
- `poise-tui` 负责本地值守、联调和操作入口
- `poise-protocol` 负责 `server` 与 `tui` 共享 DTO

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

- 同一交易所内，同一 `symbol` 只允许一个轨道
- `track_id` 是显式配置的稳定标识，不由 `symbol` 派生
- HTTP / WebSocket 以 `track_id` 作为一等标识
- SQLite 默认路径是 `.data/<environment>/poise-server.sqlite`
- Binance 适配层当前用 `mark price` 作为策略 `reference_price`

## 快速开始

### 1. 准备配置

服务端只接受 `--config <path>` 方式启动。

- 手工联调 Binance USDⓈ-M Futures 测试网时，直接复制或修改 [`configs/binance-testnet.toml`](configs/binance-testnet.toml)
- [`configs/test.toml`](configs/test.toml) 只给仓库内自动化测试使用，里面是本地假地址，不能直接拿来连 Binance

测试网最小示例如下：

```toml
environment = "testnet"
bind_address = "127.0.0.1:8000"

[exchange]
api_key = ""
api_secret = ""
rest_base_url = "https://demo-fapi.binance.com"
ws_base_url = "wss://fstream.binancefuture.com"

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 65500.0
upper_price = 67500.0
long_exposure_units = 10.0
short_exposure_units = 10.0
notional_per_unit = 375.0
```

补充说明：

- 可以继续追加 `[[tracks]]`，每个轨道都要配置唯一的 `track_id`
- 当前同一交易所内每个 `symbol` 只能出现一次
- `environment` 只决定数据目录和环境名，不自动切换交易所地址
- 真实启动时必须显式配置 `exchange.rest_base_url`、`exchange.ws_base_url`、`exchange.api_key`、`exchange.api_secret`
- 当前实现启动时一定会建立用户流、拉取 server time、持仓和挂单，所以空凭证会在启动阶段直接失败
- 示例里的 `btc-core` 区间总带宽是 `2000 USD`，在线性模式下等效每格约 `100 USD`
- 联调前要按当前测试网价格手动平移这个区间

### 2. 启动服务端

`Poise` 服务端通过 `poise-server` 二进制启动。

```bash
cargo run -p poise-server -- --config configs/binance-testnet.toml
```

服务端启动后会：

- 读取配置中的全部网格
- 初始化 SQLite
- 建立 HTTP / WebSocket 控制面
- 接入 Binance 市场数据和用户流

### 3. 启动 TUI

`Poise` 终端界面通过 `poise-tui` 二进制启动。

```bash
cargo run -p poise-tui
```

默认连接：

- HTTP：`http://127.0.0.1:8000`
- WebSocket：`ws://127.0.0.1:8000/ws`

如果要改地址，可以在启动前设置：

环境变量使用 `POISE_BASE_URL` 和 `POISE_WS_URL`。

```bash
export POISE_BASE_URL=http://127.0.0.1:9000
export POISE_WS_URL=ws://127.0.0.1:9000/ws
cargo run -p poise-tui
```

`poise-tui` 会先请求 `/tracks`，再加载当前轨道详情，并订阅 `/ws`。

### 4. 用 HTTP 快速确认

```bash
curl http://127.0.0.1:8000/tracks
curl http://127.0.0.1:8000/tracks/btc-core
```

## 当前协议

当前对外接口只有 4 个入口：

- `GET /tracks`
- `GET /tracks/:id`
- `POST /tracks/:id/commands`
- `GET /ws`

字段和错误语义见 [`docs/protocol-contract.md`](docs/protocol-contract.md)。

## 数据

- 服务端按 `environment` 使用单个 SQLite 文件保存全部轨道状态

## 开发与验证

常用命令：

```bash
cargo test -p poise-storage
cargo test -p poise-server
cargo test -p poise-tui
cargo test
```

最近一次完整验证已通过 `cargo test`。

## 当前文档

- [`docs/protocol-contract.md`](docs/protocol-contract.md)：当前 HTTP / WebSocket 协议
- [`docs/grid-strategy-product-theory-research.md`](docs/grid-strategy-product-theory-research.md)：当前策略研究与产品侧约束
- [`docs/superpowers/specs/2026-03-26-grid-phase2-application-projection-design.md`](docs/superpowers/specs/2026-03-26-grid-phase2-application-projection-design.md)：当前架构 spec
- [`docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md`](docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md)：当前策略族模型设计
- [`docs/superpowers/plans/2026-03-25-grid-platform-architecture-convergence.md`](docs/superpowers/plans/2026-03-25-grid-platform-architecture-convergence.md)：Poise 当前收敛计划与验收标准
