# Poise

`Poise` 是一个面向 Binance / Bybit USDⓈ-M Futures、Hyperliquid Perps 和 OKX SWAP 永续合约的探索型策略运行项目。当前主线把每个 track 定义为价格带内的目标占用函数，并通过库存执行器持续把实际仓位拉回目标仓位。

## 项目状态

Poise 是个人策略运行实验项目。源码公开主要为了透明记录和自用复现；项目不承诺稳定接口、兼容旧配置或响应外部需求。旧方案可能随时被删除，使用者需要自行阅读代码、配置和测试来判断是否适合自己的账户。

本项目可能连接真实合约账户并自动下单。它不构成投资建议，也不保证收益、风控效果或可用性。默认建议只在 testnet、只使用独立低权限 API key 或 Hyperliquid API wallet、只投入可以承受全部损失的资金。不要给 API key 开启提现权限，不要把 Hyperliquid 主钱包私钥交给 Poise，不要把实例目录、真实配置、SQLite 数据库、日志或密钥提交到仓库。

项目仍在快速探索，旧方案不会保留兼容层。当前文档保留这些入口：

- 本文件：启动、配置和常用开发入口。
- [docs/system-overview.md](docs/system-overview.md)：从零构建路径、当前系统边界、运行语义和事实源。
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

Hyperliquid 默认 perpetuals 和 HIP-3 builder-deployed perpetuals 可以在同一实例中混配。默认 perpetuals 使用 `BTC`、`ETH` 这类 coin 名称；HIP-3 使用交易所 wire name，例如 `xyz:CBRS`。

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

一份最小可读的配置形状如下。示例刻意不列出默认项，完整可调字段见后面的配置参考。

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
daily_loss_limit = 375.0
total_loss_limit = 750.0
```

配置由一个 `[exchange]`、一个或多个 `[[tracks]]`，以及可选的 `[account_monitor]` 组成。`[tracks.risk_acquisition]` 是某个 track 的子表，必须紧跟它所属的 `[[tracks]]` 后面；不需要调默认值时不要写。

顶层配置：

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `bind_address` | `127.0.0.1:8000` | HTTP / WebSocket 监听地址。 |
| `[exchange]` | 必填 | 当前实例连接的交易所；一个实例只能连接一个交易所。 |
| `[[tracks]]` | 必填 | 策略 track 列表；同一实例内 `track_id` 必须唯一，`venue + symbol` 必须唯一。 |
| `[account_monitor]` | 全部字段有默认值 | 账户级风险提示阈值。 |

`[exchange]` 配置：

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `venue` | 必填 | 支持 `binance`、`bybit`、`hyperliquid`、`okx`。 |
| `deployment` | Binance / Bybit / Hyperliquid 默认 `testnet`；OKX 默认 `demo` | Binance / Bybit / Hyperliquid 支持 `testnet`、`mainnet`；OKX 支持 `demo`、`mainnet`。Binance 还支持 `{ custom = { rest_base_url = "...", ws_root_base_url = "..." } }`。 |
| `api_key` | 无 | Binance / Bybit / OKX 必填，空字符串会被视为缺失。 |
| `api_secret` | 无 | Binance / Bybit / OKX 必填，空字符串会被视为缺失。 |
| `passphrase` | 无 | OKX 必填。 |
| `private_key` | 无 | Hyperliquid 必填，必须是 `0x` 加 64 个 hex 字符。 |
| `wallet_address` | 无 | Hyperliquid 必填，必须是 `0x` 加 40 个 hex 字符。 |
| `vault_address` | 无 | Hyperliquid 可选，必须是 `0x` 加 40 个 hex 字符。 |

`[[tracks]]` 配置：

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `track_id` | 必填 | 稳定业务标识，不由 `symbol` 派生。 |
| `symbol` | 必填 | Binance / Bybit 使用 `BTCUSDT` 这类合约符号；Hyperliquid 默认 perpetuals 使用 `BTC`、`ETH` 这类 coin 名称，HIP-3 perpetuals 使用 `xyz:CBRS` 这类 `{dex}:{coin}` wire name；OKX SWAP 使用 `BTC-USDT-SWAP` 这类 instrument id。 |
| `lower_price` | 必填 | 价格带下沿，必须小于 `upper_price`。 |
| `upper_price` | 必填 | 价格带上沿。 |
| `long_exposure_units` | 必填 | 价格到达下沿时的多头目标容量，必须非负。 |
| `short_exposure_units` | 必填 | 价格到达上沿时的空头目标容量，必须非负；多空容量不能同时为 `0`。 |
| `notional_per_unit` | 必填 | 每个 exposure unit 对应的名义金额，必须大于 `0`。 |
| `daily_loss_limit` | 必填 | 当日亏损终止阈值，必须大于 `0`。 |
| `total_loss_limit` | 必填 | 累计亏损终止阈值，必须大于 `0`。 |
| `min_rebalance_units` | `0.5` | 最小调仓单位；目标变化小于该值时不触发新的执行动作。 |
| `shape_family` | `linear` | 曲线形状，支持 `linear`、`inertial`、`responsive`。旧名 `concave` / `convex` 不再接受。 |
| `out_of_band_policy` | `freeze` | 价格离开主价格带后的处理策略，见下面的带外配置。 |
| `max_notional` | `max(long_exposure_units, short_exposure_units) * notional_per_unit` | track 最大绝对名义金额，必须大于 `0`。 |
| `leverage` | `10` | 启动时按 track 下发到交易所；这是 server-owned startup-only 配置，不进入策略曲线。 |
| `tick_timeout_secs` | `30` | 策略价格超过该秒数未更新时视为 stale，自动执行会等待新价格。 |
| `[tracks.risk_acquisition]` | 全部字段有默认值 | 增加风险暴露时的延迟获取参数，见下面的风险暴露获取配置。 |

`out_of_band_policy` 是单字段枚举，支持三种策略：

- `freeze`：离开主价格带后冻结目标，价格回到主带后恢复跟随曲线。
- `flatten`：离开主价格带后自动降到 `0`，可配置触发和恢复确认。
- `terminate`：离开主价格带后进入终态，目标收敛到 `0`，不会自动恢复。

常用写法：

```toml
out_of_band_policy = "freeze"
out_of_band_policy = "terminate"

# flatten 简写等价于 trigger/recover 都使用 500 bps 确认
out_of_band_policy = "flatten"

# flatten 完整对象形式
out_of_band_policy = { flatten = { trigger = { flatten_confirm = { bps = 500 } }, recover = { reentry_confirm = { bps = 500 } } } }

# 也可以使用立即触发和回到主带即恢复
out_of_band_policy = { flatten = { trigger = "immediate", recover = "back_in_band" } }
```

`flatten.trigger` 支持 `immediate` 和 `{ flatten_confirm = { bps = <整数> } }`。确认距离按价格带宽度计算，例如 `bps = 500` 表示离开边界后再走出价格带宽度的 `5%` 才触发 flatten。

`flatten.recover` 支持 `back_in_band` 和 `{ reentry_confirm = { bps = <整数> } }`。`back_in_band` 是回到主价格带就恢复；`reentry_confirm` 是回到主带后还要再进入价格带宽度的对应比例才恢复。

`[tracks.risk_acquisition]` 默认启用，但默认值不需要写进配置。它只影响增加风险暴露：增加时允许等待价格优势，降低风险暴露时优先执行。

```toml
[[tracks]]
track_id = "btc-core"
# ...这个 track 的其他字段...

[tracks.risk_acquisition]
initial_ratio = 0.3
advantage_steps = 2.0
min_release_steps = 1.0
max_release_steps = 4.0
catchup_ratio = 0.25
stale_release_minutes = 30.0
```

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `initial_ratio` | `0.3` | 从零启动或重新进入同方向 backlog 时，先允许建立目标暴露的一部分；如果目标不小于 `min_rebalance_units`，至少释放一个最小调仓单位。取值范围是 `(0, 1]`。 |
| `advantage_steps` | `2.0` | 等待曲线目标相对锚点多走多少个 `min_rebalance_units` 后释放 backlog。 |
| `min_release_steps` | `1.0` | 每次释放 backlog 的最小单位倍数。 |
| `max_release_steps` | `4.0` | 每次释放 backlog 的最大单位倍数，必须大于等于 `min_release_steps`。 |
| `catchup_ratio` | `0.25` | backlog 较大时，每次按 backlog 的这个比例尝试追赶，再受最小/最大释放单位限制。取值范围是 `(0, 1]`。 |
| `stale_release_minutes` | `30.0` | 同一个锚点等待达到这个分钟数后，即使价格还没走到优势阈值，也按同一套释放数量规则释放一批 backlog。设为 `0` 表示关闭时间释放。 |

注意 `risk_acquisition` 必须写成 `[tracks.risk_acquisition]` 子表，不要写成 `risk_acquisition = { ... }` 行内对象。多个 `[[tracks]]` 时，每个 track 都可以有自己的 `[tracks.risk_acquisition]`，子表归属于它前面最近的那个 `[[tracks]]`。

`[account_monitor]` 可选配置：

| 字段 | 默认 | 说明 |
| --- | --- | --- |
| `day_change_attention_pct` | `-3.0` | 当日权益变化比例低于该值时进入 attention。 |
| `day_change_critical_pct` | `-5.0` | 当日权益变化比例低于该值时进入 critical。 |
| `available_ratio_attention_pct` | `30.0` | 可用保证金比例低于该值时进入 attention。 |
| `available_ratio_critical_pct` | `15.0` | 可用保证金比例低于该值时进入 critical。 |
| `unrealized_loss_attention_pct` | `-5.0` | 未实现盈亏比例低于该值时进入 attention。 |
| `unrealized_loss_critical_pct` | `-10.0` | 未实现盈亏比例低于该值时进入 critical。 |

每组 account monitor 阈值都要求 attention 大于等于 critical。

当前交易所边界：

- Hyperliquid 当前只接入 perpetuals，不支持 spot、提现、划转、TWAP 或 vault 运维功能。unified account / portfolio margin 下，Poise 使用 `spotClearinghouseState` 的 USDC 可用余额作为启动保证金口径；standard mode 下使用 perps `clearinghouseState.withdrawable`。
- OKX 当前只接入 `SWAP` 永续合约，只支持 `cross` 保证金模式和 `net` 持仓模式，不支持 spot、期权、划转、提现或资金账户操作。

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

脚本默认每次都会重建同名 zellij session：先删除同名 session，再 `--forget` 旧保存态，并按当前 layout 新建。这样可以避免电脑重启后复活旧 pane 命令。若只是想附加到当前活跃 session：

```bash
./scripts/start-instance-zellij.sh --attach
# 或
POISE_ZELLIJ_ATTACH=1 ./scripts/start-instance-zellij.sh
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
