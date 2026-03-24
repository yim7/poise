# 网格平台

`grid-platform` 是一个面向 Binance USDⓈ-M Futures 的网格交易平台底座。当前仓库聚焦两件事：

- 把 `service` 做成唯一的领域状态中心和控制面宿主
- 把 `tui` 做成本地运维、验证和联调入口

两者通过统一的 HTTP / WebSocket 协议协作，SQLite 用于可选的持久化、恢复和审计。当前仓库已经完成本地内存模式、SQLite 恢复链路、`replay / paper / testnet` 验证基线、TUI 中英文切换、多实例固定区间网格，以及 `K9` 主网运行安全底座。

## 当前状态

- 当前主线：`K10` 单实例 `7x24` 值守硬化
- 最近完成：`K9` 主网运行安全底座；多实例固定区间网格；`K8` TUI 中英文切换
- 当前边界：继续采用“实例感知，单实例验收”，多实例能力已并入主线，但本阶段只要求一个已配置实例达到 mainnet 自动运行与本地值守标准
- 当前缺口：`K10` 的重复启动保护、统一健康状态、值守告警与恢复路径还未开始实现

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

### 1. 准备配置文件

服务端当前标准入口是 `--config <path>`。先创建一个环境配置文件，例如 `configs/paper.toml`：

```toml
environment = "paper"
default_symbol = "BTCUSDT"

[[instances]]
symbol = "BTCUSDT"

[instances.range]
lower_price = 90000
upper_price = 110000
grid_levels = 6
max_position_notional = 3000
```

如果要同时启动多个实例，继续追加 `[[instances]]` 即可：

```toml
[[instances]]
symbol = "ETHUSDT"

[instances.range]
lower_price = 2200
upper_price = 2800
grid_levels = 8
max_position_notional = 4000
```

配置文件模式下：

- `environment` 决定运行环境和默认数据目录
- `service` 会读取 `[[instances]]` 中的所有标的
- `default_symbol` 决定旧兼容路由和 TUI 默认选中的标的
- 每个实例的 SQLite 会自动落到 `.data/<environment>/<symbol-lowercase>.db`
- 网格使用固定区间梯子模型：价格在区间内才挂单，区间外空仓时进入等待态

### 2. 启动服务端

```bash
cargo run -p grid-platform-service -- --config configs/paper.toml
```

标准行为：

- 默认监听 `127.0.0.1:8000`
- 读取配置文件中的全部实例
- 为每个实例分别建立 SQLite 存储
- 未显式开启 `GRID_PLATFORM_BINANCE_ENABLED=1` 时，不连接真实 Binance

### 3. 另开终端启动 TUI

```bash
cargo run -p grid-platform-tui
```

默认会连接：

- HTTP：`http://127.0.0.1:8000`
- WebSocket：`ws://127.0.0.1:8000/ws`

TUI 启动后会先拉取 `/instances`，选择 `default_symbol`，再连接该标的的实例作用域快照和 WebSocket。运行中可以在实例列表里切换当前查看的标的，其余详情页保持单实例视角。快捷键：

- `[`：切到上一个实例
- `]`：切到下一个实例

### 4. 快速确认服务可用

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

### 配置文件模式下的数据路径

配置文件模式不使用单一的 `GRID_PLATFORM_SERVICE_DB_PATH`。服务端会按环境和实例名自动推导每个实例自己的 SQLite 路径：

- `paper`：`.data/paper/<symbol-lowercase>.db`
- `testnet`：`.data/testnet/<symbol-lowercase>.db`
- `mainnet`：`.data/mainnet/<symbol-lowercase>.db`

例如 `BTCUSDT` 会落到 `.data/<environment>/btcusdt.db`。

### 配置文件模式下接 Binance

```bash
cargo run -p grid-platform-service -- --config configs/testnet.toml
```

共享环境变量既可以直接 `export`，也可以写到仓库根目录 `.env`。例如：

```dotenv
GRID_PLATFORM_BINANCE_ENABLED=1
GRID_PLATFORM_BINANCE_API_KEY=your_api_key
GRID_PLATFORM_BINANCE_API_SECRET=your_api_secret
```

也可以直接从 [`.env.example`](.env.example) 复制一份再按本地环境调整。

补充说明：

- 运行环境由配置文件里的 `environment` 决定，而不是 `GRID_PLATFORM_BINANCE_ENV`
- 实例 `symbol` 由配置文件里的 `[[instances]]` 决定，而不是 `GRID_PLATFORM_BINANCE_SYMBOL`
- `GRID_PLATFORM_BINANCE_REST_BASE_URL` 和 `GRID_PLATFORM_BINANCE_WS_BASE_URL` 仍可作为共享覆盖项
- `service` 启动时会自动尝试加载仓库根目录 `.env`；同名进程环境变量优先于 `.env`
- 未配置 API Key 时，用户流不会建立，但市场元数据与市场流仍可用于联调
- mainnet 启动前必须收集签名持仓快照和签名挂单快照，因此需要同时配置 `GRID_PLATFORM_BINANCE_API_KEY` 与 `GRID_PLATFORM_BINANCE_API_SECRET`
- mainnet 启动前会执行启动对账；发现交易所持仓或交易所挂单与本地持久化状态明显不一致时，会先进入暂停态而不是继续自动下单
- `K9` 已完成 mainnet 准入安全基线；`K10` 值守硬化尚未实现，因此当前 README 只说明准入与启动安全，不把 `7x24` 值守能力写成已完成

### 单实例兼容启动路径

无配置文件启动仍然保留给回归测试和最小本地验证，但不再是当前项目的标准入口：

```bash
export GRID_PLATFORM_SERVICE_DB_PATH=.tmp/grid-platform.db
cargo run -p grid-platform-service
```

兼容路径下：

- 默认监听 `127.0.0.1:8000`
- 默认 SQLite 路径可按 `.data/<mode>/<instance_id>.db` 推导
- 默认 `instance_id` 为 `local`，可通过 `GRID_PLATFORM_INSTANCE_ID` 覆盖
- 若使用 `GRID_PLATFORM_BINANCE_ENV=mainnet`，仍要求显式设置 `GRID_PLATFORM_ALLOW_MAINNET=1`

### 自定义服务端与 TUI 连接地址

服务端：

```bash
export GRID_PLATFORM_SERVICE_ADDR=127.0.0.1:9000
cargo run -p grid-platform-service -- --config configs/paper.toml
```

TUI：

```bash
export GRID_PLATFORM_BASE_URL=http://127.0.0.1:9000
export GRID_PLATFORM_WS_URL=ws://127.0.0.1:9000/ws
cargo run -p grid-platform-tui
```

这两个变量也可以写到仓库根目录 `.env`。`tui` 启动时会自动尝试加载 `.env`，同名进程环境变量优先。

## 开发与验证

运行全部测试：

```bash
cargo test
```

如果只想先验证服务端控制面：

```bash
cargo test -p grid-platform-service --test control_plane -- --nocapture
```

最近一次完整验证见 [`TODO.md`](TODO.md)；当前主线已经确认通过的命令包括：

- `cargo test -p grid-platform-service --lib`
- `cargo test -p grid-platform-service --test cli -- --nocapture`
- `cargo test -p grid-platform-service --test mainnet_bootstrap -- --nocapture`
- `cargo test -p grid-platform-service --test control_plane -- --nocapture`
- `cargo test -p grid-platform-service --test persistence_recovery -- --nocapture`
- `cargo test -p grid-platform-service --test kernel_flow -- --nocapture`
- `cargo test -p grid-platform-service --test multi_instance_control_plane`
- `cargo test -p grid-platform-tui --test instance_switching`
- `cargo test -p grid-platform-tui --test local_paper_e2e`
- `cargo test -p grid-platform-tui`
- `cargo test`

## 文档入口

- [`docs/technical-architecture.md`](docs/technical-architecture.md)：系统边界、职责划分与运行时模型
- [`docs/protocol-contract.md`](docs/protocol-contract.md)：`service` 与 `tui` 当前共享的 HTTP / WebSocket 协议
- [`docs/binance-integration.md`](docs/binance-integration.md)：真实 Binance 接入说明
- [`docs/k6-validation.md`](docs/k6-validation.md)：replay / paper / testnet 验证手册
- [`docs/grid-strategy-product-theory-research.md`](docs/grid-strategy-product-theory-research.md)：业界成熟网格产品与学术理论调研
- [`docs/roadmap.md`](docs/roadmap.md)：阶段目标与里程碑
- [`docs/plan.md`](docs/plan.md)：近期计划与验收标准
- [`TODO.md`](TODO.md)：当前任务清单与最近验证结果

## 项目说明

- 当前仓库是一个 Rust workspace，包含 `service` 和 `tui` 两个 crate
- `service` 是唯一的服务端状态中心，未来 Web UI 也会复用同一套控制面
- `tui` 通过 `instances + snapshot + incremental events` 模型消费服务端能力
- 对外协议约束同时以 Rust 类型定义和线协议语义文档为准
