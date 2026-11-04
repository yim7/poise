# Poise

> 在冷静时决定，在慌乱中成交。
>
> *Decide in calm. Fill in chaos.*

价位、批次和观点失效价，都在想清楚的时候定好；成交多半发生在急跌急涨、人最容易乱的时候。Poise 守住这两个时刻之间的镇定：判断由人在平静时做，纪律由程序在波动中执行。

Poise 在 Hyperliquid 上自动运行分批主仓加网格的策略：按价格范围分批挂单积累主仓并保留，网格部分低买高卖循环；可以做多、做空或中性，可选按时间限速积累；带保证金风控、观点失效止损（交易所上另挂兜底止损单）、市价平仓和飞书提醒。

设计见 [分批主仓与网格设计](docs/layered-grid-design.md)，术语见 [GLOSSARY.md](GLOSSARY.md)，架构和运行语义见 [系统说明](docs/system-overview.md)。

## 当前状态

- 只支持 Hyperliquid，账户必须是 Unified 模式。
- 品种：主市场的加密货币（例如 `BTC`），以及第三方市场 `xyz` 上的黄金、原油、美股（`xyz:GOLD`、`xyz:CL`、`xyz:TSLA`）。只支持全仓、USDC 抵押。
- 连测试网还是正式网由启动文件的 `network` 决定；正式网要你自己把正式网 API 钱包私钥写进启动文件。
- 正式网上走过：新建、预览、确认启动、暂停、恢复、停止。只在测试网上验证过：接管已有仓位、观点失效减仓、市价平仓（含不足最低金额的零头）、归档后仍有仓位的读取、资金预检、保证金率读取、资金费读取、第三方市场的 `xyz:TSLA`，以及兜底单的挂出、随参数调整、暂停保留、停止撤销和常驻运行。兜底单在交易所上真正触发的路径只在模拟交易所里测过。

## 实例与启动文件

实例放在 `~/.poise/` 下，每个实例一个子目录，例如 `~/.poise/testnet/`、`~/.poise/mainnet/`。环境变量 `POISE_HOME` 可以换根目录。每个实例目录里放启动文件 `poise.toml`（权限必须是 600，程序只读不写）：

```toml
network = "testnet"
account_address = "0x…"          # 主账户地址
api_wallet_private_key = "0x…"   # 在 Hyperliquid 网页上为这个账户授权的 API 钱包私钥
data_dir = "data"                # 数据库放在这里（相对实例目录）
listen = "127.0.0.1:8010"        # 网页控制台地址，只接受本机地址
feishu_webhook = "https://open.feishu.cn/open-apis/bot/v2/hook/…"   # 可选：提醒推到这个飞书机器人
```

- 私钥由你自己写进文件；程序不生成、不保存、不打印私钥。API 钱包只能代为签名交易，不能转账或提现。
- 启动时逐项校验：文件权限、私钥格式、由私钥推出的地址是否在这个账户已授权且未过期的 API 钱包里、账户模式是否 Unified。API 钱包剩余有效期少于 14 天时提醒续期。
- `feishu_webhook` 可以不写，不写就不推提醒。地址本身就是凭据，和私钥一样只放在启动文件里，不进日志。
- 同一个实例同时只能有一个进程（实例锁）。几个实例可以同时运行，各用各的 `listen` 端口。
- 数据库第一次用时记下网络和账户，之后换网络或账户就拒绝启动：测试网和正式网要用各自的数据目录（`data_dir`），测试网的策略不会被拿到正式网上恢复运行。

## 运行

```bash
cargo build --release
(cd console && npm ci && npm run build)
```

```bash
target/release/poise instances                    # 列出 ~/.poise 下的实例：网络、监听地址、账户
target/release/poise --instance testnet check     # 只做启动校验
target/release/poise --instance testnet serve     # 常驻服务 + 网页控制台
target/release/poise --instance testnet status    # 策略、订单、仓位、可用余额
target/release/poise --instance testnet stop      # 紧急用：暂停这个标的上的策略、撤掉本系统的网格挂单（兜底止损单留着）
target/release/poise --instance testnet notify-test   # 往飞书发一条测试消息
target/release/poise --instance testnet service install   # 常驻运行（见下）
```

`--instance` 写名字就到 `~/.poise/` 下找；写路径（含 `/`，或以 `.`、`~` 开头）就照原样用那个目录。必须写：不写时报错并列出已有实例，不会按当前目录去找——正式网实例的私钥就在启动文件里，不能因为在哪个目录下执行而连错实例。

`stop` 在常驻服务运行时通过它的本机接口（启动文件里的 `listen`）暂停，服务没在跑时直接操作数据库。失效中的策略它不动：那是正在执行的止损，要放弃减仓请在网页上点停止。

`serve` 启动后：

- 把重启前还在运行的策略落到「恢复中」，遗留订单过期、撤掉、对完账之后才重新挂单；
- 按节拍（`--step-interval-secs`，默认 5 秒）驱动策略；
- 每 10 分钟单独读一次资金费率和资金费结算；
- 在 `listen` 地址提供网页控制台（静态文件默认取 `console/dist`）。

## 常驻运行

`service install` 把当前的 `poise` 程序和网页控制台（`console/dist`）复制进实例目录的 `bin/`、`console/`，写一份 launchd 用户级配置 `~/Library/LaunchAgents/com.poise.<实例名>.plist` 并启动：登录后自动启动，崩溃后 10 秒内自动拉起，运行期间用 `caffeinate` 挡住系统睡眠（合上笔记本盖子挡不住）。日志在实例目录的 `logs/poise.log`。

```bash
target/release/poise --instance mainnet service install     # 装上并启动；换新版本时重新构建后再跑一次
target/release/poise --instance mainnet service status      # 在不在跑、进程号、拉起过几次
target/release/poise --instance mainnet service uninstall   # 停掉并卸下
```

复制而不是直接指向仓库里的构建产物：仓库里重新编译、`cargo clean` 都不会悄悄换掉正在跑的程序。

## 兜底止损单与提醒

- **兜底单**：设了观点失效价的一侧，交易所上另挂一张只减仓的止损市价单，触发价在失效价外 1%，数量是仓位超出剩余数量 R 的部分，随仓位自动调整。poise 在线时失效减仓先走；poise 掉线时由交易所执行，之后 poise 发现它成交过就转失效收尾。暂停时留着，停止、失效、平仓时撤掉。详情页「兜底止损」一栏显示它。
- **飞书提醒**：越过失效价、已失效、已平仓、自动暂停（仓位或成交对不上、操作次数超限）、兜底单触发或挂不上、保证金率越过提醒或停止阈值、同一价格反复撤挂没成交、驱动连续 5 分钟出问题（和恢复）、服务启动和退出。消息只带标的、价格和原因，不带仓位、数量和金额。

## 网页控制台

- **新建**：填价格范围、最大仓位、主仓比例和批数、可选的主仓倾斜（末批÷首批，越往下买得越多的金字塔；设了它网格按全侧均匀分）、网格间距和价差、限速间隔、观点失效价，保存成草稿。
- **预览**：批次表、格子表、第一步会挂出的订单和会立即成交的订单、满仓压力测试（保证金率、强平价、建议最大仓位）、交易所上已有的其他挂单。标的上已有仓位时按接管规则分配。
- **确认启动**：带着预览令牌确认；仓位、可用资金或立即成交的订单变了，预览作废要重新看。
- **详情**：状态和在等什么、批次表、格子表、在场挂单、参数版本、保证金率、资金费率与累计资金费、今日操作次数。
- **操作**：暂停、恢复、停止；运行中可以改限速间隔和观点失效设置。停止确认之后再问是否市价平仓：只减仓单按标记价让出 5%，撤单收尾之后平掉标的上的净仓位，没平完下一步接着发。
- **仍有仓位**：已归档的标的上还有仓位时，常驻服务每分钟读一次，列表上单独一栏显示，可以直接市价平仓。
- **账户风控设置**：保证金率提醒和停止阈值、行情过期时间、资金费率提醒阈值。
- 顶栏显示当前网络和账户地址，正式网用醒目颜色标出。每一屏有自己的地址，刷新后回到原处。

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
- [console/](console/)：网页控制台（React + TypeScript + Vite，shadcn/ui + Tailwind CSS，TanStack Query）。
- [docs/](docs/)：设计与系统说明。
