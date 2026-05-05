# Poise

`Poise` 是一个面向 Binance / Bybit USDⓈ-M Futures、Hyperliquid Perps 和 OKX SWAP 永续合约的探索型策略运行项目。当前主线把每个 track 定义为价格带内的目标占用函数，并通过库存执行器持续把实际仓位拉回目标仓位。

## 项目状态

Poise 是个人策略运行实验项目。源码公开主要为了透明记录和自用复现；项目不承诺稳定接口、兼容旧配置或响应外部需求。旧方案可能随时被删除，使用者需要自行阅读代码、配置和测试来判断是否适合自己的账户。

本项目可能连接真实合约账户并自动下单。它不构成投资建议，也不保证收益、风控效果或可用性。默认建议只在 testnet、只使用独立低权限 API key 或 Hyperliquid API wallet、只投入可以承受全部损失的资金。不要给 API key 开启提现权限，不要把 Hyperliquid 主钱包私钥交给 Poise，不要把实例目录、真实配置、SQLite 数据库、日志或密钥提交到仓库。

项目仍在快速探索，旧方案不会保留兼容层。当前文档保留这些入口：

- 本文件：启动、配置和常用开发入口。
- [docs/system-overview.md](docs/system-overview.md)：当前系统边界、运行语义和事实源。
- [SECURITY.md](SECURITY.md)：安全边界、密钥使用和漏洞报告方式。
- [LICENSE](LICENSE)：源码许可证。

## 工作区结构

- [core/](core/)：领域模型、策略参数、风险规则、领域事件。
- [engine/](engine/)：track 运行时、状态机、目标计算、执行规划和恢复。
- [application/](application/)：用例服务、读模型、持久化 port 和定义索引。
- [storage/](storage/)：SQLite 持久化适配。
- [protocol/](protocol/)：`server` 与 `tui` 共享的 HTTP / WebSocket DTO。
- [exchanges/binance/](exchanges/binance/)：Binance USDⓈ-M Futures 适配。
- [exchanges/bybit/](exchanges/bybit/)：Bybit USDⓈ-M Futures 适配。
- [exchanges/hyperliquid/](exchanges/hyperliquid/)：Hyperliquid Perps 适配。
- [exchanges/okx/](exchanges/okx/)：OKX SWAP 永续合约适配。
- [server/](server/)：配置、装配、HTTP / WebSocket、runtime task 和交易所接入。
- [tui/](tui/)：本地值守和操作界面。

## 当前约束

- 一个服务实例只连接一个交易所。
- `track_id` 是显式配置的稳定业务标识，不由 `symbol` 派生。
- 同一实例内 `track_id` 必须唯一。
- 同一实例内 `venue + symbol` 必须唯一。
- HTTP / WebSocket 以 `track_id` 作为一等标识。
- SQLite 默认路径是 `<instance-dir>/.data/poise-server.sqlite`。
- 启动遇到当前配置与持久业务状态不兼容时会失败，需要操作者显式处理实例目录或数据库。

## 快速开始

### 1. 准备实例目录

服务端必须通过 `--instance-dir <path>` 启动，并从实例目录读取 `config.toml`。

```bash
mkdir -p "$HOME/poise-instances/testnet"
cp configs/demo.toml "$HOME/poise-instances/testnet/config.toml"
```

把实例目录里的 `[exchange]` 改成当前实例要连接的交易所和凭证。一个实例只能选择一个交易所。

### 2. 选择交易所

Binance USDⓈ-M Futures：

```toml
[exchange]
venue = "binance"
deployment = "testnet"
api_key = ""
api_secret = ""
```

Bybit USDⓈ-M Futures：

```toml
[exchange]
venue = "bybit"
deployment = "testnet"
api_key = ""
api_secret = ""
```

Hyperliquid Perps：

```toml
[exchange]
venue = "hyperliquid"
deployment = "testnet"
private_key = "0x..."
wallet_address = "0x..."
# vault_address = "0x..."
```

OKX SWAP 永续合约：

```toml
[exchange]
venue = "okx"
deployment = "demo"
api_key = ""
api_secret = ""
passphrase = ""
```

### 3. 配置 track

一份最小可读的配置形状如下：

```toml
bind_address = "127.0.0.1:8000"

[exchange]
venue = "binance"
deployment = "testnet"
api_key = ""
api_secret = ""

[[tracks]]
track_id = "btc-core"
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
leverage = 10
daily_loss_limit = 375.0
total_loss_limit = 750.0
tick_timeout_secs = 30
```

当前配置语义：

- `exchange.venue` 支持 `binance`、`bybit`、`hyperliquid` 和 `okx`。
- Binance / Bybit / Hyperliquid 的 `exchange.deployment` 支持 `testnet` 和 `mainnet`；OKX 支持 `demo` 和 `mainnet`。
- Binance / Bybit 使用 `api_key` 和 `api_secret`。
- Hyperliquid 使用 `private_key` 和 `wallet_address`，可选 `vault_address`；这些字段必须是 `0x` 前缀 hex。
- OKX 使用 `api_key`、`api_secret` 和 `passphrase`。
- Binance / Bybit 的 `symbol` 使用交易所合约符号，例如 `BTCUSDT`。
- Hyperliquid 只接入 perpetuals，`symbol` 使用 coin 名称，例如 `BTC` 或 `ETH`，不是 `BTCUSDT`。
- Hyperliquid unified account / portfolio margin 下，Poise 使用 `spotClearinghouseState` 的 USDC 可用余额作为启动保证金口径；standard mode 下使用 perps `clearinghouseState.withdrawable`。
- OKX 只接入 `SWAP` 永续合约，`symbol` 使用 OKX instrument id，例如 `BTC-USDT-SWAP`；当前只支持 `cross` 保证金模式和 `net` 持仓模式。
- Hyperliquid 当前不支持 spot、提现、划转、TWAP 或 vault 运维功能。
- OKX 当前不支持 spot、期权、划转、提现或资金账户操作。
- `leverage` 不写时默认 `10`，启动时会按 track 下发到交易所。
- `shape_family` 支持 `linear`、`inertial`、`responsive`。
- `out_of_band_policy` 支持 `freeze`、`flatten`、`terminate`。
- `flatten` 可用对象形式配置 `trigger` 和 `recover`。
- `min_rebalance_units` 是触发下一次执行动作的最小目标变化。

详细边界见 [docs/system-overview.md](docs/system-overview.md)。

### 4. 启动服务端

```bash
cargo run -p poise-server -- --instance-dir "$HOME/poise-instances/testnet"
```

启动后服务端会读取配置、初始化 SQLite、连接交易所、执行 startup-only 杠杆设置、恢复本地业务状态，并启动 HTTP / WebSocket 控制面。

### 5. 启动 TUI

```bash
cargo run -p poise-tui
```

默认连接：

- HTTP：`http://127.0.0.1:8000`
- WebSocket：`ws://127.0.0.1:8000/ws`

如果要改地址：

```bash
export POISE_BASE_URL=http://127.0.0.1:9000
cargo run -p poise-tui
```

### 6. HTTP 快速确认

```bash
curl http://127.0.0.1:8000/health
curl http://127.0.0.1:8000/account
curl http://127.0.0.1:8000/tracks
curl http://127.0.0.1:8000/tracks/btc-core
```

当前公开入口：

- `GET /health`
- `GET /account`
- `GET /tracks`
- `GET /tracks/:id`
- `POST /tracks/:id/commands`
- `GET /debug/tracks/:id/diagnostics`
- `GET /ws`

协议 DTO 以 [protocol/src/lib.rs](protocol/src/lib.rs) 和序列化测试为准，不再维护单独协议文档。

## zellij 值守

本地连续跑测试网实例时，可以用仓库里的 zellij 脚本托管 server、TUI 和 health probe。

```bash
export POISE_INSTANCE_DIR="$HOME/poise-instances/testnet"
./scripts/start-instance-zellij.sh
```

常用环境变量：

```bash
export POISE_INSTANCE_DIR="$HOME/poise-instances/testnet"
export POISE_BASE_URL=http://127.0.0.1:8000
export POISE_LOG_DIR="$HOME/poise-instances/testnet/.logs"
export POISE_HEALTH_FAILURE_THRESHOLD=3
export POISE_TUI_LOG="$HOME/poise-instances/testnet/.logs/poise-tui.log"
export POISE_ZELLIJ_SESSION_NAME=poise-testnet
./scripts/start-instance-zellij.sh
```

日志默认写到 `<instance-dir>/.logs/`：

- `poise-server.log`
- `poise-tui.log`
- `health-probe.log`

只看脚本参数，不启动：

```bash
POISE_INSTANCE_DIR="$HOME/poise-instances/testnet" ./scripts/start-instance-zellij.sh --dry-run
POISE_INSTANCE_DIR="$HOME/poise-instances/testnet" ./scripts/run-instance-server.sh --dry-run
POISE_INSTANCE_DIR="$HOME/poise-instances/testnet" ./scripts/run-instance-tui.sh --dry-run
POISE_BASE_URL=http://127.0.0.1:8000 ./scripts/probe-health.sh --dry-run
```

## 开发与验证

默认优先跑与改动直接相关的最小测试。常用入口：

```bash
cargo test -p poise-core
cargo test -p poise-engine
cargo test -p poise-application
cargo test -p poise-hyperliquid
cargo test -p poise-server
cargo test -p poise-tui
```
