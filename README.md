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
- SQLite 默认路径是 `<instance-dir>/.data/<environment>/poise-server.sqlite`
- Binance 适配层当前用 `mark price` 作为策略 `reference_price`

## 快速开始

### 1. 准备实例目录

服务端启动时必须传 `--instance-dir <path>`，并从该目录读取 `config.toml`。每个实例目录对应一个独立运行实例，配置、数据库、日志和状态备份都落在这个目录下面。

手工联调 Binance USDⓈ-M Futures 测试网时，先准备实例目录：

```bash
mkdir -p "$HOME/poise-instances/testnet-demo"
cp configs/binance-testnet.demo.toml "$HOME/poise-instances/testnet-demo/config.toml"
```

补充说明：

- [`configs/binance-testnet.demo.toml`](configs/binance-testnet.demo.toml) 和 [`configs/binance-mainnet.demo.toml`](configs/binance-mainnet.demo.toml) 只作为模板
- [`configs/test.demo.toml`](configs/test.demo.toml) 只给仓库内自动化测试和示例参考使用，里面是本地假地址，不能直接拿来连 Binance
- 真实运行时推荐把本地凭证和实例配置都放在实例目录，不再把 `*.local.toml` 留在仓库配置目录里

下面给的是一份字段完整的 `track` 示例，当前支持的参数都显式写出来：

```toml
environment = "testnet"
bind_address = "127.0.0.1:8000"

[exchange]
api_key = ""
api_secret = ""

[[tracks]]
track_id = "btc-core"
venue = "binance"
symbol = "BTCUSDT"
lower_price = 65500.0
upper_price = 67500.0
long_exposure_units = 10.0
short_exposure_units = 10.0
notional_per_unit = 375.0
min_rebalance_units = 0.5
shape_family = "linear"
out_of_band_policy = "freeze"
max_notional = 3750.0
daily_loss_limit = 375.0
total_loss_limit = 750.0
tick_timeout_secs = 30
```

补充说明：

- 可以继续追加 `[[tracks]]`，每个轨道都要配置唯一的 `track_id`
- 当前同一交易所内每个 `symbol` 只能出现一次
- `environment = "testnet"` 时，服务端固定接 Binance USDⓈ-M Futures 测试网地址
- `environment = "mainnet"` 时，服务端固定接 Binance USDⓈ-M Futures 主网地址
- `environment = "test"` 只保留给仓库内自动化测试，不用于真实运行
- 真实启动时必须显式配置 `exchange.api_key`、`exchange.api_secret`
- 当前实现启动时一定会建立用户流、拉取 server time、持仓和挂单，所以空凭证会在启动阶段直接失败
- 风控参数会在启动阶段校验：`max_notional > 0`、`daily_loss_limit > 0`、`total_loss_limit > 0`
- 示例里的 `btc-core` 区间总带宽是 `2000 USD`，在线性模式下等效每格约 `100 USD`
- 联调前要按当前测试网价格手动平移这个区间
- `min_rebalance_units` 当前表示“触发下一次执行动作的最小目标变化”，不再只是 `current_exposure -> latest_target` 的停手阈值
- 没有活动生命周期时，`min_rebalance_units` 的参考点是 `current_exposure`
- 存在 `SubmitPending` 或 `Working` 时，`min_rebalance_units` 的参考点是当前执行目标 `working_order.desired_exposure`
- 当最新目标相对当前执行目标的漂移仍低于门槛时，系统会继续当前生命周期：
  - 已有 `SubmitPending` 会继续执行，不会因为小幅 target 漂移被连续 supersede
  - 已有 `Working` 不会因为小幅 target 漂移被 cancel-replace
- 如果需要更频繁跟随最新目标，应调低该值；如果希望执行更稳、减少 supersede / cancel-replace，应调高该值

### 2. 启动服务端

`Poise` 服务端通过 `poise-server` 二进制启动。

把 `$HOME/poise-instances/testnet-demo/config.toml` 里的 `exchange.api_key` 和 `exchange.api_secret` 改成你自己的测试网凭证，然后按默认严格模式启动：

```bash
cargo run -p poise-server -- --instance-dir "$HOME/poise-instances/testnet-demo"
```

如果当前本地 SQLite 快照和新的配置不一致，默认会拒绝启动。这时如果你确认要丢弃旧本地快照，并按交易所真实仓位和挂单重建本地状态，再加 `--rebuild-state`：

```bash
cargo run -p poise-server -- --instance-dir "$HOME/poise-instances/testnet-demo" --rebuild-state
```

`--rebuild-state` 的语义是：

- 先备份当前 `<instance-dir>/.data/<environment>/poise-server.sqlite`
- 删除旧本地快照对应的 SQLite sidecar 文件
- 用当前配置重新初始化本地状态
- 启动后再按交易所真实仓位和挂单继续接管

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

如果要改地址，可以在启动前设置 `POISE_BASE_URL`：

```bash
export POISE_BASE_URL=http://127.0.0.1:9000
cargo run -p poise-tui
```

`poise-tui` 会先请求 `/tracks`，再加载当前轨道详情，并从 `POISE_BASE_URL` 自动推导 `/ws` 订阅地址。

### 4. 用 HTTP 快速确认

```bash
curl http://127.0.0.1:8000/health
curl http://127.0.0.1:8000/tracks
curl http://127.0.0.1:8000/tracks/btc-core
```

## 当前协议

当前对外接口只有 5 个入口：

- `GET /health`
- `GET /tracks`
- `GET /tracks/:id`
- `POST /tracks/:id/commands`
- `GET /ws`

`GET /health` 的语义：

- `200`：当前全部轨道都没有 `attention_required`
- `503`：至少一个轨道出现 `stale market data` 或 `recovery anomaly`
- 响应体包含 `status`、`track_count`、`attention_required_count`

字段和错误语义见 [`docs/protocol-contract.md`](docs/protocol-contract.md)。

## 数据

- 服务端按实例目录和 `environment` 使用单个 SQLite 文件保存全部轨道状态

多账号主网运行时，目录应该显式分开，例如：

```text
~/poise-instances/mainnet-account-a/
  config.toml
  .data/
  .logs/

~/poise-instances/mainnet-account-b/
  config.toml
  .data/
  .logs/
```

只要实例目录不同，即使两个配置都写 `environment = "mainnet"`，数据库和日志也不会共享。

## 开发与验证

常用命令：

```bash
cargo test -p poise-storage
cargo test -p poise-server
cargo test -p poise-tui
cargo test
```

日常本地检查建议直接跑：

```bash
./scripts/check-workspace.sh
```

默认快路径只覆盖生产代码 lint，以及除 `poise-server` / `poise-tui` bin 单测外的 workspace 单元 / 集成测试，不包含：

- `poise-server` 的 bin 单元测试
- `poise-tui` 的 bin 单元测试
- 测试目标的 `clippy`
- workspace doctest
- `poise-tui` 的 3 个慢速真实端到端测试

如果你需要把 `poise-tui` 的慢速真实端到端测试也一起验收，再跑：

```bash
./scripts/check-workspace.sh --full
```

`--full` 会切换到全量检查，补齐：

- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace --all-targets`
- `cargo test --workspace --doc`
- `cargo test -p poise-tui --bin poise-tui real_server_ -- --ignored`

## 用 zellij 连续跑模拟仓

这套方式适合本机连续值守测试网。它解决的是“会话托管”和“固定巡检”，不替代系统级 supervisor。

仓库内置了 4 个运行资产：

- [`scripts/start-instance-zellij.sh`](scripts/start-instance-zellij.sh)：创建或附着到 `zellij` session
- [`scripts/run-instance-tui.sh`](scripts/run-instance-tui.sh)：启动 `poise-tui`
- [`scripts/run-instance-server.sh`](scripts/run-instance-server.sh)：启动 `poise-server` 并把日志落到实例目录
- [`scripts/probe-health.sh`](scripts/probe-health.sh)：循环探测 `GET /health`

对应布局文件在 [`ops/zellij/poise-instance.kdl`](ops/zellij/poise-instance.kdl)。

### 1. 先准备本地配置

```bash
mkdir -p "$HOME/poise-instances/testnet-demo"
cp configs/binance-testnet.demo.toml "$HOME/poise-instances/testnet-demo/config.toml"
```

把 `$HOME/poise-instances/testnet-demo/config.toml` 里的 `exchange.api_key` 和 `exchange.api_secret` 改成你自己的测试网凭证。

### 2. 启动 zellij 会话

先确保本机已经安装 `zellij`，然后执行：

```bash
export POISE_INSTANCE_DIR="$HOME/poise-instances/testnet-demo"
./scripts/start-instance-zellij.sh
```

默认会创建或附着到名为 `poise-testnet-demo` 的 session。布局里有 3 个 pane：

- 左侧主 pane：`poise-tui`
- 右上：`poise-server`
- 右下：`/health` 巡检

### 3. 常用环境变量

如果你想改默认值，可以在启动前设置：

```bash
export POISE_INSTANCE_DIR="$HOME/poise-instances/testnet-demo"
export POISE_BASE_URL=http://127.0.0.1:8000
export POISE_LOG_DIR="$HOME/poise-instances/testnet-demo/.logs"
export POISE_REBUILD_STATE=0
export POISE_HEALTH_FAILURE_THRESHOLD=3
export POISE_TUI_LOG="$HOME/poise-instances/testnet-demo/.logs/poise-tui.log"
export POISE_ZELLIJ_SESSION_NAME=poise-testnet-demo
./scripts/start-instance-zellij.sh
```

如果你要通过脚本方式重建本地状态，可以把 `POISE_REBUILD_STATE=1`，这样 `run-instance-server.sh` 会自动在 `poise-server` 启动命令后追加 `--rebuild-state`：

```bash
export POISE_INSTANCE_DIR="$HOME/poise-instances/testnet-demo"
export POISE_REBUILD_STATE=1
./scripts/run-instance-server.sh
```

如果你想在连续失败达到阈值时触发外部通知，还可以额外设置：

```bash
export POISE_HEALTH_ALERT_HOOK='printf "alert:%s:%s\n" "$POISE_HEALTH_FAILURE_COUNT" "$POISE_HEALTH_LAST_STATUS"'
```

### 4. 日志位置

默认日志目录是 `<instance-dir>/.logs/`，主要看这三个文件：

- `$HOME/poise-instances/testnet-demo/.logs/poise-tui.log`
- `$HOME/poise-instances/testnet-demo/.logs/poise-server.log`
- `$HOME/poise-instances/testnet-demo/.logs/health-probe.log`

### 5. 巡检脚本

单次探测只需要 `POISE_BASE_URL`：

```bash
POISE_BASE_URL=http://127.0.0.1:8000 ./scripts/probe-health.sh --once
```

只看脚本会用什么参数，不真正启动：

```bash
POISE_INSTANCE_DIR="$HOME/poise-instances/testnet-demo" ./scripts/start-instance-zellij.sh --dry-run
POISE_INSTANCE_DIR="$HOME/poise-instances/testnet-demo" ./scripts/run-instance-server.sh --dry-run
POISE_INSTANCE_DIR="$HOME/poise-instances/testnet-demo" ./scripts/run-instance-tui.sh --dry-run
POISE_BASE_URL=http://127.0.0.1:8000 ./scripts/probe-health.sh --dry-run
```

### 6. 会话管理

列出当前 session：

```bash
zellij list-sessions
```

重新附着：

```bash
zellij attach poise-testnet-demo
```

结束这套模拟仓会话：

```bash
zellij kill-sessions poise-testnet-demo
```

## 当前文档

- [`docs/protocol-contract.md`](docs/protocol-contract.md)：当前 HTTP / WebSocket 协议
- [`docs/grid-strategy-product-theory-research.md`](docs/grid-strategy-product-theory-research.md)：当前策略研究与产品侧约束
- [`docs/superpowers/specs/2026-03-26-grid-phase2-application-projection-design.md`](docs/superpowers/specs/2026-03-26-grid-phase2-application-projection-design.md)：当前架构 spec
- [`docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md`](docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md)：当前策略族模型设计
- [`docs/superpowers/plans/2026-03-25-grid-platform-architecture-convergence.md`](docs/superpowers/plans/2026-03-25-grid-platform-architecture-convergence.md)：Poise 当前收敛计划与验收标准
