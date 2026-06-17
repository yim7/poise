# Poise 后续 Goal 执行清单

## 状态

本文是后续可执行 goal 队列，不是长期架构事实源。长期架构和运行语义仍以 [system-overview.md](system-overview.md) 为准。

本文的生命周期：

- 执行中的 goal 可以暂时保留在本文，作为任务拆分、验收和 commit 记录。
- goal 完成后，仍成立的架构、运行语义和操作规则必须吸收到 [system-overview.md](system-overview.md) 或经确认拆出的长期模块文档。
- 已完成且已吸收的阶段性 task 记录应删除，不作为长期文档保留。
- 如果本文内容与 [system-overview.md](system-overview.md) 冲突，以 [system-overview.md](system-overview.md) 为准，并优先修正文档结构。

执行本文中的 goal 时遵守项目约定：

- 每个 goal 开始前先检查工作区状态，不能把不相关改动混进同一提交。
- 每个 task 先补或调整验收测试，再实现。
- 默认先跑与改动直接相关的最小测试，只有影响面扩大时才升级到 crate 或 workspace 级测试。
- 每个 task 验收通过后立即提交，并把 commit SHA 回写到本文。
- 如果实现中发现需要新增明显的跨层抽象、改变已确认语义，或当前边界放不下，先停止并确认。

## 排序原则

重要性优先级高于实现难度。排序依据：

- P0：保护真实交易事实和资金安全。漏成交、错误 PNL、错误下单、错误恢复优先处理。
- P1：降低操作错误和排查成本。让启动、重启、配置、健康状态可解释。
- P2：提升策略调参质量。先用数据回答风险和收益问题，再增加策略能力。
- P3：扩展使用场景。多 track、多交易所、组合视角放在稳定性之后。

实现难度标记：

- S：主要是文档、投影、测试或局部逻辑。
- M：需要跨 2 到 3 个 crate，但边界清楚。
- L：需要跨 runtime、storage、exchange 或 UI 的完整闭环。

## Goal 总览

| 顺序 | Goal | 优先级 | 难度 | 状态 | 依赖 |
| --- | --- | --- | --- | --- | --- |
| 0 | 收尾当前 PNL backfill | P0 | M | completed | 无 |
| 1 | 增加运行健康与启动前检查 | P0 | M | completed | Goal 0 |
| 2 | 建立交易事实对账闭环 | P0 | L | completed | Goal 0, Goal 1 |
| 3 | 提供配置解释和 dry-run | P1 | M | completed | Goal 1 |
| 4 | 建立 BTC-USD-SWAP 调参 replay | P1 | M | completed | Goal 3 |
| 5 | 文档治理与模块拆分决策 | P1 | S | pending | Goal 0, Goal 1 |
| 6 | 增加账户视角分析层 | P2 | M | pending | Goal 2, Goal 3 |
| 7 | 评估多 track 和更多交易所 | P3 | L | pending | Goal 1 到 Goal 4 |

## Goal 0. 收尾当前 PNL backfill

目标：把真实成交 PNL 漏计问题处理完整，保证重启或 WebSocket 漏消息后能通过 OKX recent fills 回补本地 PNL 明细。

为什么先做：这是已经发生过的真实问题，直接影响盈亏统计和止损判断。当前工作区已有未提交实现，执行前先核对 diff，不要混入其他方向。

改动范围：

- `engine/src/ports.rs`
- `exchanges/okx/src/rest/`
- `exchanges/okx/src/mapper.rs`
- `exchanges/okx/src/connected.rs`
- `server/src/runtime/`
- `server/src/assembly.rs`
- `server/src/main.rs`
- `docs/system-overview.md`

Task 清单：

| Task | 状态 | 验收 | Commit |
| --- | --- | --- | --- |
| 0.1 核对并补齐 OKX recent fills 到 `TrackPnlRecord` 的映射 | completed | OKX REST fill mapper 覆盖 inverse 数量、PNL asset、fee asset、source key 幂等 | `9521162` |
| 0.2 接入 runtime PNL backfill task | completed | backfill task 启动后可写入缺失 PNL，重复执行不重复入账，失败只降级为告警 | `37cb992` |
| 0.3 更新 PNL 文档语义 | completed | `system-overview.md` 不再描述 `pnl_asset` 只由公开读模型推导 | `4ec3cad` |

建议验证命令：

```bash
cargo test -p poise-okx trade_fill
cargo test -p poise-okx recent_track_pnl_records
cargo test -p poise-server pnl_backfill::tests::
cargo test -p poise-server assembly::tests::
cargo build -p poise-server
git diff --check
```

设计停点：

- 如果 recent fills 无法稳定映射到现有 `TrackPnlRecord`，不要引入新的临时 PNL 表，先确认事实模型。
- 如果需要跨交易所统一 backfill trait，先保持 OKX 局部实现，等 Goal 2 再评估抽象。

## Goal 1. 增加运行健康与启动前检查

目标：让系统在启动前和运行中能明确回答“能不能安全交易”和“现在哪个子系统不健康”。

为什么排第二：真实交易系统不能只靠日志和值守经验。当前 OKX 币本位已经跑起来，下一步应降低手工排查和误操作成本。

改动范围：

- `server/src/runtime/`
- `server/src/exchange_startup.rs`
- `server/src/http.rs`
- `protocol/src/lib.rs`
- 必要时扩展 `application` read model

Task 清单：

| Task | 状态 | 验收 | Commit |
| --- | --- | --- | --- |
| 1.1 定义健康状态模型 | completed | 能表达 market data、user data、effect worker、recovery、account monitor、PNL backfill 的状态 | `e477f6f` |
| 1.2 增加启动前 preflight | completed | 启动前校验账户模式、保证金模式、symbol metadata、mark price、available balance、最小交易单位 | `475385d`, `624939d` |
| 1.3 暴露健康查询接口 | completed | HTTP 能返回整体状态、每个 task 最近成功时间、最近错误摘要 | `f745def` |
| 1.4 增加启动失败和降级路径测试 | completed | 缺 mark price、错误 position mode、缺 metadata 时错误信息明确 | `eefb708` |

建议验证命令：

```bash
cargo test -p poise-server exchange_startup::tests::
cargo test -p poise-server assembly::tests::
cargo test -p poise-server config::tests::
cargo test -p poise-protocol
git diff --check
```

设计停点：

- 如果健康状态需要长期持久化，先确认哪些是事实、哪些只是当前进程观测。
- 如果为了 `/health` 引入复杂状态总线，先停止。第一版可以用现有 runtime task 汇报的轻量 snapshot。

## Goal 2. 建立交易事实对账闭环

目标：把“交易所事实”和“本地持久化事实”做成可检查闭环，降低漏成交、重复入账、恢复异常的风险。

为什么重要：PNL backfill 只是补一个入口。长期看，本地订单、成交、手续费、资金费、仓位都需要能与交易所事实对齐。

改动范围：

- `engine/src/ports.rs`
- `application` 持久化 port 和 read model
- `storage/src/`
- `exchanges/okx/src/`
- `server/src/runtime/`
- `server/src/http.rs`

Task 清单：

| Task | 状态 | 验收 | Commit |
| --- | --- | --- | --- |
| 2.1 增加 PNL backfill 观测字段 | completed | 能查询最近回补时间、写入数量、跳过数量、最近错误 | `1aa2876` |
| 2.2 增加 recent fills 审计接口 | completed | 能比较最近交易所 fills 与本地 `track_pnl_records` 的覆盖情况 | `7bcd54f` |
| 2.3 接入资金费事实 | completed | 可归属到 track 的 funding fee 进入 `TrackPnlRecord`，资产不一致时拒绝混算 | `606c59b` |
| 2.4 增加启动后自动审计 | completed | 重启后能发现最近成交缺失，并给出明确 diagnostics | `8a7b999` |

建议验证命令：

```bash
cargo test -p poise-storage pnl
cargo test -p poise-application read_model::tests::
cargo test -p poise-okx
cargo test -p poise-server runtime::diagnostics::tests::
cargo test -p poise-server http::tests::
git diff --check
```

设计停点：

- 如果不同交易所的 fills 语义差异很大，不要急着抽通用模型。先为 OKX 做完整闭环，再抽取最小公共 port。
- 如果资金费无法稳定归属 track，第一版只记录到 account 层或 diagnostics，不要强行进入 track PNL。

## Goal 3. 提供配置解释和 dry-run

目标：让用户在真实启动前知道配置会产生什么仓位、交易单位、保证金占用和风险边界。

为什么重要：币本位 inverse 的 contracts、USD 面值、BTC 保证金、PNL asset 很容易混淆。配置解释比增加参数更有价值。

改动范围：

- `server/src/config.rs`
- `server/src/exchange_startup.rs`
- `server/src/http.rs` 或独立 CLI 入口
- `protocol/src/lib.rs`
- 必要时增加小型工具模块

Task 清单：

| Task | 状态 | 验收 | Commit |
| --- | --- | --- | --- |
| 3.1 增加配置解释模型 | completed | 对每个 track 展示 unit 对应 native quantity、USD 面值、最小步长、最大名义、loss limit 资产 | `68e2955` |
| 3.2 增加 dry-run 启动检查 | completed | 不下单、不订阅实时 task，也能加载配置、metadata、账户摘要并输出风险说明 | `0028165` |
| 3.3 增加当前价格下的容量估算 | completed | inverse 能展示可用 BTC、mark price、leverage、估算最大 contracts 和 USD 面值 | `5fba7de` |
| 3.4 增加错误提示测试 | completed | 缺 symbol、缺 ctVal、数量低于最小单位时提示具体字段和计算结果 | `d3862ad` |

建议验证命令：

```bash
cargo test -p poise-server config::tests::
cargo test -p poise-server exchange_startup::tests::
cargo test -p poise-protocol
git diff --check
```

设计停点：

- 如果 dry-run 需要复用完整 runtime，先停止。第一版只需要配置、metadata、账户摘要和纯计算。
- 不要把 dry-run 输出变成新的事实源，它只是解释当前配置和交易所 metadata。

## Goal 4. 建立 BTC-USD-SWAP 调参 replay

目标：用历史价格或采样价格序列，回答当前 BTC 币本位策略在不同参数下的仓位变化、成交密度、手续费拖累和风险边界。

为什么排在 dry-run 后：先解释单点配置，再做跨时间的模拟。这样 replay 可以复用已有配置解释和数量语义。

改动范围：

- 可先放在 `tools/` 或 `server/src/` 的测试型模块，具体位置执行前确认
- `core/src/strategy.rs`
- `core/src/risk.rs`
- 必要时复用 `engine` 的目标计算，不接真实 execution port

Task 清单：

| Task | 状态 | 验收 | Commit |
| --- | --- | --- | --- |
| 4.1 定义 replay 输入格式 | completed | 支持读取价格序列、初始仓位、手续费率、当前 track 配置 | `3f3e48d` |
| 4.2 输出仓位和交易统计 | completed | 输出最大 contracts、最大 USD 面值、估算 BTC 手续费、成交次数、区间内仓位分布 | `1bb5ba1` |
| 4.3 对比参数组合 | completed | 能对比 `min_rebalance_units`、杠杆、notional per unit、区间宽度的影响 | `c1a0d7d` |
| 4.4 增加固定样本验收测试 | completed | 同一输入输出稳定，inverse contracts exposure 不随价格漂移 | `00d1f71` |

建议验证命令：

```bash
cargo test -p poise-core strategy::tests:: risk::tests::
cargo test -p poise-engine runtime::tests::
git diff --check
```

设计停点：

- 不要在第一版 replay 中模拟完整交易所撮合。先用策略目标和简化成交假设回答配置风险。
- 如果 replay 需要读取真实交易所历史数据，先把数据获取和模拟计算拆开，不要混成一个实时依赖。

## Goal 5. 文档治理与模块拆分决策

目标：让长期文档、执行中 goal 文档和历史阶段性文档的边界清楚，避免出现多套互相竞争的事实源。

为什么重要：项目已经从探索进入真实运行，文档漂移会直接增加误操作概率。

默认原则：

- [system-overview.md](system-overview.md) 是长期事实入口，当前阶段不默认拆分。
- 阶段性 spec、plan、review 文档只服务开发过程；完成后删除，并把仍成立的信息吸收到长期文档。
- 只有当某类内容明显变成独立读者、独立维护节奏或篇幅过长时，才拆成长期模块。
- 拆出的模块必须在 [system-overview.md](system-overview.md) 中有明确入口、边界和事实归属，不重复维护同一语义。

第一版模块拆分判断：

- 暂不拆：架构边界、数量单位、PNL 语义、启动与运行时语义，继续放在 [system-overview.md](system-overview.md)。
- 候选拆分：OKX 值守操作手册、配置解释和 dry-run 使用手册。只有当这些内容变成较长的操作步骤，并且会频繁更新时再拆。
- 不保留：已完成的 OKX 币本位 spec / plan、阶段性设计评审、一次性调研记录。

改动范围：

- `README.md`
- `docs/system-overview.md`
- `docs/okx-coin-margined-spec.md`
- `docs/okx-coin-margined-plan.md`
- 必要时新增经确认的长期模块文档

Task 清单：

| Task | 状态 | 验收 | Commit |
| --- | --- | --- | --- |
| 5.1 更新系统概览 | completed | PNL、数量单位、OKX 运行语义与当前实现一致 | `0cfb4b7` |
| 5.2 做模块拆分决策 | pending | 明确继续单文档，或列出长期模块、边界、入口和去重规则 |  |
| 5.3 吸收并删除历史 spec/plan | pending | 已完成阶段性信息进入长期文档；旧 spec/plan 不再保留 |  |
| 5.4 更新 README 导航 | pending | README 只指向长期文档和当前有效入口，不指向历史过程文档 |  |

建议验证命令：

```bash
rg -n "pnl_asset|币本位|OKX|唯一长期文档" README.md docs
git diff --check
```

设计停点：

- 如果文档整理发现实现和已确认语义不一致，先修实现或确认语义，不要只改文档掩盖差异。

## Goal 6. 增加账户视角分析层

目标：在不改变 engine 的前提下，提供账户层的持仓、合约、PNL 和净暴露分析。

为什么不是更早做：这是用户决策视角，不是 engine 的交易事实。应该在事实对账和配置解释稳定后做。

改动范围：

- `application` read model
- `server` projector
- `protocol`
- `tui`

Task 清单：

| Task | 状态 | 验收 | Commit |
| --- | --- | --- | --- |
| 6.1 定义账户分析 read model | pending | 展示 settlement asset、合约张数、USD 面值、折算 BTC 暴露、PNL asset |  |
| 6.2 增加 hedge-like 视图 | pending | 能展示用户现货 BTC 与合约净暴露的估算关系，但不影响 engine 决策 |  |
| 6.3 接入 TUI 或 HTTP detail | pending | 用户能从现有界面看到净暴露和账户视角风险 |  |

建议验证命令：

```bash
cargo test -p poise-application read_model::tests::
cargo test -p poise-server projector::tests:: http::tests::
cargo test -p poise-protocol
pnpm exec tsc -b
git diff --check
```

设计停点：

- 不要把“对冲目的”写进 core 策略目标。账户分析只能是 read model 或 UI 层解释。
- 如果需要用户输入现货余额来源，先用配置或账户摘要明确事实来源，不要隐式假设。

## Goal 7. 评估多 track 和更多交易所

目标：在 OKX 单实例可靠后，再评估多 track 容量分配和交易所扩展。

为什么最后做：这是范围扩展。当前更大的收益来自把单实例真实运行做稳。

候选方向：

- 多 track 共享账户容量和风险预算。
- OKX 以外交易所的 recent fills / funding fee backfill 能力。
- 更完整的 TUI / workbench 配置编辑和模拟。
- 多实例运行管理。

进入条件：

- Goal 0 到 Goal 4 已完成。
- OKX 单实例能稳定运行并能解释成交、PNL、恢复和健康状态。
- 新方向有明确用户场景，不只是为了抽象完整。

设计停点：

- 如果多交易所支持要求提前统一所有 exchange 语义，先停止。只抽已经被两个真实实现验证过的最小公共接口。
- 如果多 track 容量分配会改变当前单 track 风险语义，必须先写 spec。
