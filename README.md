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

- 手工联调 Binance USDⓈ-M Futures 测试网时，先复制 [`configs/binance-testnet.demo.toml`](configs/binance-testnet.demo.toml) 为本地未跟踪文件，例如 `configs/binance-testnet.local.toml`
- [`configs/test.demo.toml`](configs/test.demo.toml) 只给仓库内自动化测试和示例参考使用，里面是本地假地址，不能直接拿来连 Binance
- 仓库默认忽略 `configs/*.local.toml`

测试网最小示例如下：

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
```

补充说明：

- 可以继续追加 `[[tracks]]`，每个轨道都要配置唯一的 `track_id`
- 当前同一交易所内每个 `symbol` 只能出现一次
- `environment = "testnet"` 时，服务端固定接 Binance USDⓈ-M Futures 测试网地址
- `environment = "mainnet"` 时，服务端固定接 Binance USDⓈ-M Futures 主网地址
- `environment = "test"` 只保留给仓库内自动化测试，不用于真实运行
- 真实启动时必须显式配置 `exchange.api_key`、`exchange.api_secret`
- 当前实现启动时一定会建立用户流、拉取 server time、持仓和挂单，所以空凭证会在启动阶段直接失败
- 风控参数会在启动阶段校验：`max_notional > 0`、`daily_loss_limit < 0`、`stop_loss_pct > 0`
- 示例里的 `btc-core` 区间总带宽是 `2000 USD`，在线性模式下等效每格约 `100 USD`
- 联调前要按当前测试网价格手动平移这个区间

### 2. 启动服务端

`Poise` 服务端通过 `poise-server` 二进制启动。

```bash
cp configs/binance-testnet.demo.toml configs/binance-testnet.local.toml
```

把 `configs/binance-testnet.local.toml` 里的 `exchange.api_key` 和 `exchange.api_secret` 改成你自己的测试网凭证，然后启动：

```bash
cargo run -p poise-server -- --config configs/binance-testnet.local.toml
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

## 用 zellij 连续跑模拟仓

这套方式适合本机连续值守测试网。它解决的是“会话托管”和“固定巡检”，不替代系统级 supervisor。

仓库内置了 3 个运行资产：

- [`scripts/start-paper-zellij.sh`](scripts/start-paper-zellij.sh)：创建或附着到 `zellij` session
- [`scripts/run-paper-tui.sh`](scripts/run-paper-tui.sh)：启动 `poise-tui`
- [`scripts/run-paper-server.sh`](scripts/run-paper-server.sh)：启动 `poise-server` 并把日志落到本地
- [`scripts/probe-health.sh`](scripts/probe-health.sh)：循环探测 `GET /health`

对应布局文件在 [`ops/zellij/poise-paper.kdl`](ops/zellij/poise-paper.kdl)。

### 1. 先准备本地配置

```bash
cp configs/binance-testnet.demo.toml configs/binance-testnet.local.toml
```

把 `configs/binance-testnet.local.toml` 里的 `exchange.api_key` 和 `exchange.api_secret` 改成你自己的测试网凭证。

### 2. 启动 zellij 会话

先确保本机已经安装 `zellij`，然后执行：

```bash
./scripts/start-paper-zellij.sh
```

默认会创建或附着到名为 `poise-paper` 的 session。布局里有 3 个 pane：

- 左侧主 pane：`poise-tui`
- 右上：`poise-server`
- 右下：`/health` 巡检

### 3. 常用环境变量

如果你想改默认值，可以在启动前设置：

```bash
export POISE_CONFIG_PATH=configs/binance-testnet.local.toml
export POISE_HEALTH_BASE_URL=http://127.0.0.1:8000
export POISE_LOG_DIR=.logs/paper
export POISE_HEALTH_FAILURE_THRESHOLD=3
export POISE_TUI_LOG=.logs/paper/poise-tui.log
export POISE_ZELLIJ_SESSION_NAME=poise-paper
./scripts/start-paper-zellij.sh
```

如果你想在连续失败达到阈值时触发外部通知，还可以额外设置：

```bash
export POISE_HEALTH_ALERT_HOOK='printf "alert:%s:%s\n" "$POISE_HEALTH_FAILURE_COUNT" "$POISE_HEALTH_LAST_STATUS"'
```

### 4. 日志位置

默认日志目录是 `.logs/paper/`，主要看这两个文件：

- `.logs/paper/poise-tui.log`
- `.logs/paper/poise-server.log`
- `.logs/paper/health-probe.log`

### 5. 巡检脚本

手工单次探测：

```bash
./scripts/probe-health.sh --once
```

只看脚本会用什么参数，不真正启动：

```bash
./scripts/start-paper-zellij.sh --dry-run
./scripts/run-paper-server.sh --dry-run
./scripts/probe-health.sh --dry-run
```

### 6. 会话管理

列出当前 session：

```bash
zellij list-sessions
```

重新附着：

```bash
zellij attach poise-paper
```

结束这套模拟仓会话：

```bash
zellij kill-sessions poise-paper
```

## 当前文档

- [`docs/protocol-contract.md`](docs/protocol-contract.md)：当前 HTTP / WebSocket 协议
- [`docs/grid-strategy-product-theory-research.md`](docs/grid-strategy-product-theory-research.md)：当前策略研究与产品侧约束
- [`docs/superpowers/specs/2026-03-26-grid-phase2-application-projection-design.md`](docs/superpowers/specs/2026-03-26-grid-phase2-application-projection-design.md)：当前架构 spec
- [`docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md`](docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md)：当前策略族模型设计
- [`docs/superpowers/plans/2026-03-25-grid-platform-architecture-convergence.md`](docs/superpowers/plans/2026-03-25-grid-platform-architecture-convergence.md)：Poise 当前收敛计划与验收标准
