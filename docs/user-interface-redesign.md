# Poise 用户界面与在线配置改造设计

## 文档状态

本文是 2026-09-08 已确认并进入实施的目标设计与任务清单。它不把尚未完成的目标描述成当前能力，也不是长期架构事实源。当前系统仍以 [system-overview.md](system-overview.md) 和代码为准。

本文用于在实现前固定这次改造的产品边界、运行语义、模块归属和验收标准。改造全部完成后，应把最终成立的内容更新到 `system-overview.md` 和 README，然后删除本文。旧 TUI、旧 Track Tuning Workbench 及其文档不作为兼容目标。

## 目标

Poise 需要一个面向单机运行的本地用户界面。用户可以在同一个界面中：

- 判断实例当前是否具备安全交易条件。
- 理解每个 Track 当前仓位、策略目标、风险限制、可执行目标和不动作原因。
- 查看订单绑定、PNL、运行任务与诊断信息。
- 修改现有 Track 的在线参数，预览对当前真实仓位的影响。
- 通过明确的“保存并应用”操作持久化配置并激活新定义。
- 在新配置成功激活后，让现有决策、风险与执行链自动开始调整仓位。
- 执行暂停、恢复、平仓和终止等人工控制。

“自动开始调整仓位”的准确含义是：新配置成功激活后，系统立即使用当前真实仓位和最新有效行情重新决策，并把结果交给现有 executor 与 effect queue。它不表示强制立即成交，也不能绕过 risk acquisition、账户容量、价格保护、最小调整量、订单所有权或恢复状态。

## 非目标

第一版不做以下事情：

- 不支持远程部署、多用户、账户体系、角色权限或公网访问；启用正式控制界面时，server 拒绝非 loopback 的 bind address。
- 不在浏览器中读取或写入原始 `config.toml`。
- 不把交易所密钥、private key、passphrase 或完整 exchange 配置传给前端。
- 不支持在线修改 venue、symbol、track ID、leverage、凭证、deployment endpoint 或 bind address。
- 不支持在线新增、复制、删除 Track；这些能力要等现有 Track 的配置替换语义稳定后再独立设计。
- 不提供绕过正常风险与执行路径的“立即按新目标成交”。
- 不保存旧 UI、旧草稿格式、旧 session snapshot 或旧配置编辑协议的兼容代码。
- 不把界面读模型、预览结果或 activity 记录当作运行状态事实源。

## 当前事实与缺口

当前实现已经具备：

- server 的 HTTP 查询、Track 命令和 WebSocket committed read-model 推送。
- TUI 对这些协议的本地展示与操作。
- Track Tuning Workbench 的离线编辑、确定性 evaluation 和只读 Server live inspector。
- `TradingRuntime::handle(track_id, TrackInput)` 的单 Track 串行更新与提交协议。
- `TrackDefinition`、`InstrumentSpec`、真实 `PositionQty`、executor binding 和 session effect queue。

但当前实现不具备在线配置替换：

- `TrackDefinition` 在启动时构造，运行期间没有正式替换入口。
- `TrackInput` 没有配置应用输入。
- Workbench 只保存自己的草稿，不回写实例配置，也不激活运行时定义。
- Workbench 直接读取本地配置文件并维护独立草稿，不适合作为真实交易控制入口。
- executor 的 config profile 变化会重建 ledger 并清除本地 bindings；如果直接用于在线替换而没有先解决交易所订单，会失去旧订单所有权。
- 当前 TUI 与 Workbench 同时投影相近状态，增加协议变更的修改范围和测试成本。

因此，这次工作不是给现有页面增加“保存”按钮，而是重建用户界面，并为配置应用增加一条完整、安全、可恢复的业务闭环。

## 主导复杂度

### Change amplification

相同的 Track 状态目前分散在 protocol、TUI、Workbench TypeScript 类型和两个界面中。一次 read-model 变化会触及多套展示与测试。

### Cognitive load

当前界面把 lifecycle、executor、四层 target、acquisition、bindings、策略参数和诊断信息以相近权重展示。用户必须理解内部名词，才能判断“为什么没有交易”和“现在是否安全”。

### Unknown unknowns

如果由前端分别执行写文件、暂停、撤单、替换定义和恢复，任一步失败或进程退出都会留下不明确的文件、运行时和交易所订单组合。调用顺序本身会成为外泄的系统知识。

## 方案比较

### 方案 A：保留配置文件启动语义，修改后重启

这是最小基线：只改进编辑器，保存后要求用户重启 server，由 fresh-session rebuild 使用新配置。

优点是运行时几乎不需要新概念，崩溃恢复语义清楚。缺点是不能满足保存后自动开始调整；用户还要人工协调停止、重启、检查 inherited orders 和恢复状态。这个方案保留为故障恢复路径，但不作为目标交互。

### 方案 B：继续扩展 TUI

优点是 Rust 直接复用 protocol 类型，启动和 zellij 值守简单。缺点是复杂表单、影响预览、目标关系、确认动作和异步应用进度都不适合终端布局。继续扩展会让 TUI 同时承担监控、配置编辑和工作流编排，界面与测试复杂度都会快速增加。

### 方案 C：在旧 Tauri Workbench 上增加运行控制

优点是已有 React/Tauri 工具链和部分 Server live 代码。缺点是旧 Workbench 的核心模型是“读取任意本地文件、保存独立草稿、离线评估、复制导出”，与“server 拥有当前实例配置并激活真实运行定义”不同。继续叠加会同时保留文件编辑、Tauri bridge、HTTP client、Base URL 和两套配置状态。

### 方案 D：server 托管新的本地 Web UI

server 在现有 HTTP/WebSocket 监听地址上同时提供静态前端。浏览器只访问当前实例，不直接碰文件或交易所。

该方案被选定，原因是：

- 运行状态、配置验证、持久化和激活都可以留在各自 owner 内部。
- UI 与 server 同源，不需要通用 CORS 或用户配置 Base URL。
- 不增加独立桌面进程和 Tauri bridge。
- server 停止时交易也已经停止；单独保留一个无法读取运行态的桌面壳没有额外可靠性收益。
- 可以删除 TUI 和旧 Workbench，最终只保留一套正式用户界面。

React、TypeScript、Vite 等通用构建工具可以重新选用，但不能把旧 Workbench 的页面结构、store、draft snapshot、Tauri commands 或 ServerLivePanel 当作新设计的基础。

## 最终产品结构

用户打开实例本地地址后，只看到三个一级入口：

```text
总览
Track
系统
```

不再设置独立的“调参工具”或“Server live inspector”。配置是 Track 工作区的一部分，运行状态与配置影响在同一个业务上下文中展示。

### 全局框架

```text
┌──────────────────────────────────────────────────────────────────┐
│ Poise   Hyperliquid · Connected · Ready        更新于 180ms 前 │
├──────────────┬───────────────────────────────────────────────────┤
│ 总览         │                                                   │
│              │                  当前页面                         │
│ Tracks       │                                                   │
│  BTC         │                                                   │
│  ETH         │                                                   │
│              │                                                   │
│ 系统         │                                                   │
└──────────────┴───────────────────────────────────────────────────┘
```

全局顶部状态只回答实例级问题：交易所连接、trading readiness、当前环境和最后更新时间。测试网与主网必须使用不可混淆的持续标识，不能只在首次打开时提示。

## 总览页面

总览页面按以下顺序组织：

1. 当前是否可以交易；不可交易时展示最主要 blocker 和处理入口。
2. 账户资产、保证金和风险信号。
3. Track 列表。
4. 需要处理的异常与降级任务。

每个 Track 摘要只显示完成判断所需的信息：

```text
BTC · Active
当前仓位 0.0000    可执行目标 0.0000
等待建立 1.6612 单位空头的风险释放
```

正常状态不使用大面积绿色；颜色优先表达需要关注、阻止执行和危险动作。`decision revision` 不出现在摘要中。

## Track 工作区

Track 工作区只有三个页签：

```text
运行 | 配置 | 活动
```

### 运行页

运行页首先回答“系统现在为什么这样做”。顶部使用用户可读的主状态：

```text
等待风险释放

策略希望建立 1.6612 单位空头。风险目标已经批准，
但当前 session 尚未释放新增风险，因此暂不下单。
```

原始 `DecisionReason` 仍可在诊断详情中查看，但不能替代主状态解释。

#### 仓位与目标

`current position` 是交易所观察事实，不属于四层 target 变换。页面需要在同一条有方向的 exposure 标尺中展示它与四层目标的关系：

```text
实际仓位          0.0000
策略目标         -1.6612
风险批准目标     -1.6612
风险释放边界      0.0000
当前可执行目标    0.0000

待释放：空头 1.6612
实际到执行差额：0.0000
```

前端只能绘制 server 已经提供的 committed 值，不能根据其他字段重算 target、backlog、notional 或 quantity。

#### 下一步条件

当系统等待时，在目标图旁直接展示解除条件：

```text
价格达到 79,941.74，或继续等待 57 分钟
下一次释放：空头 0.4153
释放后的执行目标：-0.4153
```

如果有多个阻塞原因，主区域显示当前真正决定行为的原因，其他诊断放入“更多限制”。

#### 订单、PNL 与配置摘要

- 展示全部 active bindings，不只展示第一条。
- 区分 submit pending、working、cancel pending 和 outcome unknown。
- PNL 明确区分 complete、incomplete 与未估值 cashflow。
- 配置摘要只显示价格区间、最大多空 exposure、每单位名义金额和 out-of-band policy；完整参数进入配置页。
- `decision revision`、原始状态枚举、anchor、session 时间和底层 diagnostics 默认折叠。

#### 人工操作

暂停、恢复、平仓和终止使用 server 返回的可用性与 disabled reason。`flatten` 和 `terminate` 必须在确认层中展示当前仓位、active orders、预期控制状态和不可逆含义。

页面不能根据 lifecycle 自行猜测按钮是否可用。

### 配置页

配置页编辑的是 server 返回的当前生效配置安全投影。第一版允许在线应用：

- `lower_price`
- `upper_price`
- `long_exposure_units`
- `short_exposure_units`
- `notional_per_unit`
- `min_rebalance_units`
- `shape_family`
- `out_of_band_policy`
- `max_notional`
- `daily_loss_limit`
- `total_loss_limit`
- `risk_acquisition`

以下字段只读并标记为需要重启：

- `track_id`
- `symbol`
- `venue`
- `leverage`
- `tick_timeout_secs`
- exchange credentials 与 deployment
- `bind_address`

在线字段是否继续增加，以实际运行语义为准，不因为表单可以编辑就自动开放。

编辑过程中页面同时展示三份信息：

- 当前生效值。
- 尚未应用的草稿值。
- server 根据当前 committed facts 生成的影响预览。

影响预览至少包括：

```text
当前策略目标               -1.6612
新策略目标                 -3.2500
当前 position quantity      0.000000 BTC
新配置下的当前 exposure      0.0000
预计目标变化                空头 1.5888 units
预计目标名义变化            317.76 USDC
预计首次可执行目标          -0.5000
限制                        risk acquisition
```

预览是基于指定时间点事实的解释，不是成交承诺。页面必须展示预览时间；点击应用时 server 使用最新事实重新验证和计算。

页面只提供三个动作：

- `放弃修改`
- `重新预览`
- `保存并应用`

“保存并应用”打开最终确认层，明确说明会更新实例配置并可能产生真实订单。不能使用含义不完整的“保存”作为真实应用按钮。

### 活动页

活动页只展示对操作判断有价值的离散事件：

- 配置应用接受、等待撤单、激活、失败。
- 人工命令。
- submit outcome unknown 与恢复结果。
- 订单终态和重要成交。
- readiness、行情或账户 gate 的变化。

行情 tick 和每次 decision revision 不进入活动流，否则重要事件会被淹没。

## 系统页面

系统页面展示：

- health 与 trading readiness，且解释两者差异。
- runtime task 状态与最近错误。
- WebSocket、market data、user data、reconcile 和 PNL backfill 状态。
- exchange、deployment、实例目录和版本等非敏感信息。
- 最近的配置文件外部变更冲突。

凭证只显示“已配置/缺失”，不能返回值、长度、尾号或可用于识别密钥的信息。

## 配置事实与版本

### 事实源

- `config.toml` 继续是当前实例下次启动时使用的完整配置事实源。
- 运行中的 committed `TrackDefinition` 是当前进程实际使用的生效定义。
- 正常状态下，两者必须一致。
- UI 草稿、预览和配置应用进度都不是持久配置事实源。

UI 不读取完整配置文件。server 只投影允许展示和编辑的 Track 字段。

### 配置版本

配置查询返回一个由当前实例全部可编辑配置的规范化内容计算的 opaque version。它是实例级 HTTP 并发令牌，不是领域 revision，也不需要显示给普通用户。任何 Track 的配置成功应用后，该 version 都会变化。

应用请求必须携带 `expected_version`：

- version 匹配时才可以接受应用。
- version 不匹配时返回冲突，并要求重新加载当前配置。
- 同一实例同时只执行一个配置应用；其他 Track 可以保留浏览器草稿，但不能同时进入应用流程。
- server 在写入前重新读取实例配置文件；如果文件被外部修改，不覆盖它，而是拒绝应用并报告外部变更。
- decision revision 不能代替 configuration version。

### 文件写入

配置文件写入由 server 内部的 TOML adapter 负责：

- 只替换目标 Track 允许在线应用的字段。
- 保留 exchange 配置、凭证、其他 Track、未知顺序和可保留的注释格式。
- 在实例目录内写入权限受限的临时文件，完成 flush 后使用同目录原子 rename 替换。
- 保持原配置文件的受限权限。
- 写入失败时不激活新定义。
- 不创建长期备份、迁移文件或双写格式。

## 配置预览与应用接口

前端只需要理解三个配置接口概念：

1. 当前配置快照。
2. 影响预览。
3. 配置应用状态。

建议的 HTTP 形状：

```text
GET  /tracks/:id/configuration
POST /tracks/:id/configuration/preview
PUT  /tracks/:id/configuration
DELETE /tracks/:id/configuration/application/:application_id
```

具体字段由 `protocol` 定义并通过序列化测试固定。核心要求如下：

- `GET` 只返回安全投影、opaque version 和当前 application status。
- `preview` 接收完整在线配置草稿，返回字段错误、当前/新目标、quantity/notional 解释、预计初始 frontier 和 blockers。
- `PUT` 接收 `application_id`、`expected_version` 和已经预览的完整草稿。
- `application_id` 由客户端生成，用于不确定响应后的幂等重试；同一个 ID 不能对应不同内容。
- `PUT` 返回“已接受”，不谎称异步撤单、对账和激活已经完成。
- `DELETE` 只表示放弃尚未持久化的申请；server 仍要先确认旧订单状态，才能恢复旧定义的执行。
- 配置应用状态和最终生效快照通过现有 WebSocket 语义发布；断线重连后也能从 `GET` 得到当前状态。

前端不能顺序调用“暂停、撤单、写配置、恢复”来实现应用。所有顺序、重试和失败处理必须隐藏在 server/application 内部。

## 配置应用运行语义

### 状态

同一实例同时最多有一个配置应用。对外只需要以下状态：

```text
idle
preparing
waiting_for_orders
activating
active
failed
```

这些状态是配置应用工作流的公开读模型，不替代 `TrackState`、executor state 或 decision reason。内部实现不必为每个状态建立公开类型或独立模块。

### 正常流程

```text
接收草稿与 expected version
  -> 使用当前定义、InstrumentSpec、PositionQty、行情和容量约束重新验证
  -> 持久化 application ID、请求内容 hash 与 accepted 结果
  -> 关闭该 Track 的新 submit
  -> 通过现有 effect 所有权协议撤销旧 bindings
  -> 等待 terminal order facts 与权威 exchange snapshot
  -> 原子写入 config.toml
  -> 安装新 TrackDefinition
  -> 用真实 PositionQty 和新 notional_per_unit 重算 current exposure
  -> 重建 config-dependent executor ledger 与自动策略 session 状态
  -> 使用最新有效行情提交第一份新 decision
  -> 恢复新 submit
```

配置应用不是一次长时间持有 mutation guard 的网络事务。等待撤单和 exchange 查询期间不能持有 Track mutation guard；它通过显式 session 状态、effect queue 和后续 `TrackInput` 推进。

### 旧订单所有权

- 接受配置应用后立即禁止基于旧定义创建新 submit。
- `SubmitPending`、`SubmitAuthorized`、`CancelAfterSubmit`、`Working`、`CancelPending` 和 outcome unknown 都视为 active binding，必须沿用现有身份和裁决协议。
- 只有所有旧 bindings 已经由权威事实解决，才能安装会重建 executor ledger 的新定义。
- 不能通过清空本地 bindings、换 client order ID 或直接调用 exchange `cancel all` 后相信 ACK 来跳过该过程。
- cancel ACK 仍不等于订单 terminal；当前 `CancelAllSucceeded` 会清除本地 working orders，因此配置应用流程不能把它当作完成条件，必须等待 `CompleteOpenOrderSnapshot` 与订单终态事实。

### 当前仓位与单位重算

`notional_per_unit` 变化时，交易所真实 position quantity 不会变化，但同一仓位对应的 exposure units 会变化。激活新定义时必须：

1. 保留最新权威 `PositionQty`。
2. 使用同一 `InstrumentSpec` 和新配置重新计算 `current_exposure`。
3. 使用重算后的 exposure 初始化 executor ledger 和 risk acquisition。
4. 在 read model 中同时展示 quantity 与 units，避免把单位变化误认为真实成交。

前端不能执行这个换算。

### 风险释放

配置激活视为新的策略 acquisition 起点：

- 新目标增加当前真实风险时，frontier 从重算后的当前 exposure 开始，不因修改配置立即授予全部新增风险。
- 新目标降低风险时，允许立即朝 risk-approved target 降低风险。
- 新目标跨零时，先允许到 `0`，再按新方向释放风险。
- 不继承旧配置的 acquisition anchor、frontier、stale timer 或 release 进度。
- PNL、真实仓位、订单事实和 durable control 不因配置应用被清空。

### 控制状态

- Track 处于自动运行时，成功激活后自动重新决策并继续运行。
- Track 已暂停时，可以保存并激活配置，但保持暂停，不自动恢复。
- Track 处于 manual flatten/override 时保留人工控制；新自动策略定义只在恢复自动控制后生效于目标计算。
- terminated Track 不接受在线配置应用；终止不是换一份配置即可恢复的状态。
- 旧配置下的 `Frozen`、`FlattenPending` 和 risk acquisition 等派生自动状态不跨配置继承；新定义从当前真实事实重新评估。

### 容量与最小交易规则

- 新定义继续使用当前 session 的权威 `InstrumentSpec`，不能从 symbol 推断 tick、lot、contract value 或资产。
- `notional_per_unit`、最大 exposure 或 `max_notional` 增加时，必须通过当前 account capacity guard。
- 容量校验必须取得与当前 `InstrumentSpec`、结算资产和观测时间对应的显式证明；现有返回 `Option<f64>` 且 `None` 放行的 gate 不能直接作为配置应用的安全依据。
- 无法证明新增风险容量时必须失败关闭并拒绝应用，不以 USDT、USDC 或 USD 等值作为默认假设。
- core/engine 提供基于当前 `InstrumentSpec` 的纯校验：由候选配置换算最大目标 quantity 和最小调整 quantity，并验证 quantity step、min quantity 与 min notional；不能等到执行规划时才发现不满足 venue 规则。
- 预览通过不保证应用时仍通过；应用使用最新 committed facts 再验证。

## 失败与进程退出

目标不是跨文件、内存和交易所建立无法实现的全局原子事务，而是保证任何中断点都不会失去订单所有权或在未知状态继续增加风险。

### 写文件前失败

- 旧配置仍是持久事实源。
- Track 保持禁止新 submit，直到旧订单完成权威对账。
- 用户可以重试应用或取消申请；取消后只有在订单状态已明确时才能恢复旧定义执行。

### 文件替换后、运行时激活前退出

- 重启读取新配置。
- fresh-session startup 重新查询真实仓位和 open orders，并按既有启动语义清理 inherited orders。
- 不恢复旧 application session state。

文件已经替换后，定义安装在完成预验证的 candidate 上执行，正常业务错误必须在写文件前被排除。如果此后仍发生内部不变量错误，executor 保持禁止新 submit，readiness 进入 critical，并要求重启按新配置 fresh-start；系统不把文件自动回滚为旧配置。

### 激活后响应丢失

- 相同 `application_id` 的重试通过 durable audit 返回同一个结果，不重复撤单或重复激活；同一个 ID 携带不同配置时显式拒绝。
- UI 重连后从配置快照确认当前 active version。

配置应用在开始任何订单或文件副作用前先记录 accepted audit。进程重启后不恢复中断的工作流，而是比较 audit 中的请求内容 hash 与当前配置：当前配置已经等于候选时把结果解释为已应用；仍是旧配置时把旧申请标记为中断，允许用户使用同一个 application ID 明确重试。系统不能仅凭“曾经 accepted”推断配置已经生效。

accepted audit 是第一版应用接口的前置能力，不是后续可选增强。存储层必须持久记录 `application_id`、请求内容 hash、旧/候选 configuration version 与结果；相同 ID、相同 hash 返回原结果，相同 ID、不同 hash 明确冲突。

### 无法解决的订单状态

- 配置应用进入 `failed` 或持续 `waiting_for_orders`。
- executor 保持禁止新 submit。
- readiness 明确报告 blocker。
- 不能自动回退为旧策略继续下单，也不能假定新配置已经生效。

## 模块归属

### core

- 继续拥有 `TrackDefinition`、`TrackConfig`、`InstrumentSpec` 和单位换算。
- 不知道 HTTP、TOML 文件、用户界面或配置应用进度。

### engine

- 拥有替换定义时如何从当前事实建立候选 runtime、重算 exposure、重建 config-dependent executor/risk state，以及生成下一次 decision。
- 生成取消旧 bindings 所需的 effects，不能执行网络请求。
- 不写配置文件。

### application

- `TradingRuntime` 继续是唯一业务更新 owner。
- 在现有 `handle(track_id, TrackInput)` 协议中接受配置应用及后续 effect/order 事实。
- 串行维护每个 Track 的 pending configuration application，并保证旧订单解决后才安装新定义。
- 通过窄的配置持久化 port 提交已经验证、准备激活的 Track 定义。
- 不解析 server TOML schema。

不新增一个与 `TradingRuntime` 并列、转发相同状态的公共 coordinator。配置应用的复杂步骤通过 `TradingRuntime` 的一个聚焦入口和内部私有实现隐藏。

### server

- 拥有实例路径、完整 `Config`/`TrackSpec`、安全字段投影和 TOML adapter。
- 提供配置查询、预览、应用 HTTP handler 和 WebSocket 投影。
- 托管前端静态资源。
- 正式控制界面只绑定 loopback，不保留 permissive CORS；如未来需要远程操作，应重新设计认证和网络边界，不能复用当前接口直接开放监听。

### protocol

- 拥有新的配置快照、预览、应用请求/状态和 WebSocket DTO。
- DTO 只表达跨进程契约，不升级为 engine 内部业务对象。

### Web UI

- 拥有表单交互、草稿、布局、确认层、连接状态和 server DTO 的展示。
- 不拥有策略、风险、quantity/notional、PNL 或目标计算。
- 不持久化 active config，不接触交易所，也不编排配置应用步骤。

### storage

- 继续保存 durable control、领域事件、PNL 和诊断历史。
- 不保存完整配置副本，也不成为 active configuration 的第二事实源。
- 记录不含敏感配置值的配置应用审计事件，供幂等判断和活动页说明发生时间、application ID、请求内容 hash、旧/新 version、结果与变化字段；该事件不用于恢复配置内容。

## 概念预算

本设计只新增三个需要跨边界理解的概念：

- `configuration version`：HTTP 并发令牌，不是领域对象。
- `configuration preview`：一次只读解释结果，不是事实源。
- `configuration application`：一个 Track 的异步应用工作流，运行时只有一个 active instance。

以下内容只表示职责，不应自动实现为公共 class、service 或 DTO：

- “配置应用模块”是 `TradingRuntime` 内部聚焦能力，不是新的顶层运行时。
- “TOML adapter”是 server 内部对配置持久化 port 的实现。
- “页面状态解释”是 server read model 与前端文案，不是新的领域状态机。

旧 Workbench draft snapshot、Tauri bridge command、Server live inspector state 和 TUI view state 在新系统中删除，不转换为新抽象。

## 验收标准

### 用户判断

- 打开总览后，不展开诊断即可判断实例是否具备交易条件。
- 打开 Track 运行页后，可以直接看出实际仓位、策略目标、风险批准目标、可执行目标、差额以及当前不动作原因。
- `decision revision` 不作为主要状态展示。
- 正常等待、风险阻塞、恢复异常和系统故障具有不同表达。

### 配置预览

- 前端提交草稿后，所有 target、quantity、notional、容量和风险释放解释都来自 server。
- 修改 `notional_per_unit` 时，预览明确展示真实 quantity 不变而 exposure units 改变。
- 非法配置、资产不匹配、venue minimum 和容量不足在应用前被拒绝。

### 保存与应用

- 成功应用会更新实例 `config.toml`，保留凭证、其他 Track、文件权限和不相关内容。
- stale version 或外部文件修改不会被静默覆盖。
- 配置有旧 active bindings 时，新定义不会在旧订单获得权威终态前开始 submit。
- 成功激活后，自动运行的 Track 使用最新行情产生新 decision，并通过正常风险与执行路径调整仓位。
- 已暂停或人工控制的 Track 不会因应用配置被隐式恢复自动交易。
- 不确定 HTTP 响应后的同 application ID 重试不会重复应用。

### 失败与恢复

- 在撤单、文件写入、定义安装和响应发送等关键边界模拟失败或进程退出，均不会失去旧订单身份或在未知状态继续增加风险。
- 配置文件已经替换时，重启只使用新配置和交易所权威事实 fresh-start，不恢复旧 application session。
- 配置文件尚未替换时，重启仍使用旧配置。

### 界面与安全

- 正式前端由 server 同源托管，只面向 loopback。
- 前端响应、日志、activity 和错误中不包含密钥或完整原始配置。
- `flatten`、`terminate` 和“保存并应用”都有与真实影响匹配的确认层。
- 浏览器刷新和 WebSocket 重连不会丢失当前配置应用状态。

### 删除旧实现

- 新界面达到运行监控、人工命令、配置预览与应用、活动和系统诊断的验收后，删除 `tui/`、TUI 启动脚本与测试。
- 删除 `tools/track-tuning-workbench/` 的旧 UI、Tauri bridge、草稿存储和旧文档。
- 如果新的配置预览继续使用 `engine::decision_evaluation`，只迁移仍然成立的 engine 能力，不保留 Workbench DTO 或 fixture 语义。
- 更新 workspace、README、脚本与 `system-overview.md`，然后删除本文。

## 实施任务清单

- [x] **U1 只读运行总览**：server 同源托管最小前端；展示 readiness、Track 摘要和新的运行页。Commit：`f1d882e`。
- [x] **U2 配置读取与预览**：从 server 读取安全配置投影，编辑一个现有 Track，并由 core/engine 使用当前 `InstrumentSpec`、真实 `PositionQty`、最新有效行情、venue minimum 与显式容量证明计算影响预览；证明缺失时失败关闭，不写文件、不改仓位。Commits：`4e0127f`、`af048c4`。
- [x] **U3 无 active binding 的保存并应用**：先拒绝全部 active binding 状态、未解决的 submit outcome、recovery anomaly 与 terminated Track，再完成 version/application ID 审计、原子 TOML patch、定义替换、position quantity 重算、新 acquisition 起点和自动重新决策。Commit：`c515f42`。
- [x] **U4 有订单的完整应用**：关闭新 submit，处理 pending/working/cancel/outcome unknown，等待权威快照后激活。Commit：`b7e8cdc`。
- [x] **U5 失败恢复与幂等**：覆盖外部文件变化、写入失败、断线、进程退出和 application ID 重试。Commit：`7f9ce2e`。
- [x] **U6 人工控制与系统页**：迁移 pause/resume/flatten/terminate、health、tasks、diagnostics 和 activity。Commit：`79d3b5f`。
- [ ] **U7 删除旧界面并更新依据**：完成真实实例验收后删除 TUI、旧 Workbench 和临时入口，更新 README 与唯一长期依据，然后删除本文。Commit：待完成。

## 测试 seams

测试只通过以下公开 seam 验证行为：

- Web UI：使用浏览器可见 DOM 和 server wire fixture 验证用户能看见、理解和操作的结果；不测试 React 私有 store、hook 或格式化函数。
- Server HTTP/WebSocket：通过真实 router 请求验证状态、配置预览、应用接受、错误和推送契约；不直接调用 handler 私有辅助函数。
- `TradingRuntime`：通过公开的 Track 输入与 committed snapshot/receipt 验证配置替换、订单所有权和重新决策；不读取或改写 engine 私有字段。
- 配置持久化 port：通过同一接口的临时实例目录 adapter 验证版本冲突、精确字段替换、权限和故障语义；不把直接读取文件作为 runtime 行为断言的替代品。
- 真实进程闭环：最终通过启动 server、浏览器交互、WebSocket 更新和测试交易所 fixture 验证纵向流程；不会对真实主网账户执行验收交易。

每个切片都必须从真实 UI 输入穿过 protocol、server、application/engine、持久化或 effect，再回到 committed read model 与页面结果，不能按“先做全部前端、再补后端”分层留下不可运行的半成品。
