# Poise

Poise 在 Hyperliquid 上自动运行分批主仓加网格的策略：按价格范围分批挂单积累主仓并保留，网格部分低买高卖循环；可以做多、做空或中性，可选按时间限速积累。交易所原生工具能做到的部分不重复造，系统只补交易所做不到的：在 Hyperliquid 上自动运行网格、限速积累，以及围绕它们的风控。

设计见 [分批主仓与网格设计](docs/layered-grid-design.md)，术语见 [GLOSSARY.md](GLOSSARY.md)，架构和运行语义见 [系统说明](docs/system-overview.md)。

## 当前状态

- 只支持 Hyperliquid，账户必须是 Unified 模式。
- 品种：主市场的加密货币（例如 `BTC`），以及第三方市场 `xyz` 上的黄金、原油、美股（`xyz:GOLD`、`xyz:CL`、`xyz:TSLA`）。只支持全仓、USDC 抵押。
- 连测试网还是正式网由启动文件的 `network` 决定；正式网要你自己把正式网 API 钱包私钥写进启动文件。
- **正式网还没有做过端到端验证**，下面这些只在测试网上验证过：新建、预览、确认启动、接管已有仓位、暂停恢复停止、观点失效减仓、资金预检、保证金率读取、资金费读取，以及第三方市场的 `xyz:TSLA`。

## 实例目录与启动文件

每个实例一个目录，里面放启动文件 `poise.toml`（权限必须是 600，程序只读不写）：

```toml
network = "testnet"
account_address = "0x…"          # 主账户地址
api_wallet_private_key = "0x…"   # 在 Hyperliquid 网页上为这个账户授权的 API 钱包私钥
data_dir = "data"                # 数据库放在这里（相对实例目录）
listen = "127.0.0.1:8010"        # 网页控制台地址，只接受本机地址
```

- 私钥由你自己写进文件；程序不生成、不保存、不打印私钥。API 钱包只能代为签名交易，不能转账或提现。
- 启动时逐项校验：文件权限、私钥格式、由私钥推出的地址是否在这个账户已授权且未过期的 API 钱包里、账户模式是否 Unified。API 钱包剩余有效期少于 14 天时提醒续期。
- 同一个实例目录同时只能有一个进程（实例锁）。
- 数据库第一次用时记下网络和账户，之后换网络或账户就拒绝启动：测试网和正式网要用各自的数据目录（`data_dir`），测试网的策略不会被拿到正式网上恢复运行。

## 运行

```bash
cargo build --release
(cd console && npm ci && npm run build)
```

```bash
target/release/poise --instance <实例目录> check     # 只做启动校验
target/release/poise --instance <实例目录> serve     # 常驻服务 + 网页控制台
target/release/poise --instance <实例目录> status    # 策略、订单、仓位、可用余额
target/release/poise --instance <实例目录> stop      # 紧急用：暂停这个标的上的策略、撤掉本系统的全部挂单
```

`serve` 启动后：

- 把重启前还在运行的策略落到「恢复中」，遗留订单过期、撤掉、对完账之后才重新挂单；
- 按节拍（`--step-interval-secs`，默认 5 秒）驱动策略；
- 每 10 分钟单独读一次资金费率和资金费结算；
- 在 `listen` 地址提供网页控制台（静态文件默认取 `console/dist`）。

## 网页控制台

- **新建**：填价格范围、最大仓位、主仓比例和批数、网格间距和价差、限速间隔、观点失效价，保存成草稿。
- **预览**：批次表、格子表、第一步会挂出的订单和会立即成交的订单、满仓压力测试（保证金率、强平价、建议最大仓位）、交易所上已有的其他挂单。标的上已有仓位时按接管规则分配。
- **确认启动**：带着预览令牌确认；仓位、可用资金或立即成交的订单变了，预览作废要重新看。
- **详情**：状态和在等什么、批次表、格子表、在场挂单、参数版本、保证金率、资金费率与累计资金费、今日操作次数。
- **操作**：暂停、恢复、停止；运行中可以改限速间隔和观点失效设置。
- **账户风控设置**：保证金率提醒和停止阈值、行情过期时间、资金费率提醒阈值。

## 开发

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd console && npx tsc --noEmit && npx vitest run && npm run build)
```

前端开发服务器把 `/api` 转给 `POISE_API`（默认 `http://127.0.0.1:8010`）：

```bash
(cd console && npm run dev)
```

## 目录

- [strategy/](strategy/)：纯计算。参数与校验、批次和格子划分、规划、判定、记账、限速、接管、资金预检、满仓压力测试、观点失效。不碰网络和数据库。
- [runtime/](runtime/)：唯一写入入口。驱动策略一步一步走（读取、判定、执行），SQLite 存储，网页接口，常驻服务，交易所和账户接口，以及测试用的模拟交易所。
- [hyperliquid/](hyperliquid/)：Hyperliquid 适配：签名、下单撤单、订单与成交查询、账户读数。
- [cli/](cli/)：命令行 `poise`。
- [console/](console/)：网页控制台（React + Vite）。
- [docs/](docs/)：设计与系统说明。
