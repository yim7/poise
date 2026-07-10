# Poise 可靠性与可解释性优化执行清单

## 状态

本文是当前已确认的实现任务清单，不是长期架构事实源。长期架构和运行语义仍以 [system-overview.md](system-overview.md) 为准。

当前状态：执行中。当前 task 是 0.8 区分 submit final failure 与 outcome unknown；Task 1.2 暂停，等待 0.8 完成。

本文的生命周期：

- 执行中的 goal 暂时保留在本文，记录任务拆分、验收和 commit SHA。
- goal 完成后，把仍成立的架构和运行语义吸收到 [system-overview.md](system-overview.md)。
- 已完成且已吸收的任务记录从本文删除，不把历史执行过程保留成第二套事实源。
- 如果实现中发现本文与当前代码事实冲突，先停止执行并修正方案，不能用代码迁就错误方案。

执行约束：

- 每个 task 开始前检查工作区状态，不混入不相关改动。
- 每个 task 先补失败的验收测试，再做最小实现。
- 默认运行直接相关的最小测试；只有跨 crate 验收或本轮最终验收时才扩大范围。
- 每个 task 验收通过后立即提交，并把 commit SHA 回写本文；未完成提交和回写，不开始下一个 task。
- 一次提交只处理当前 task 的一个业务问题。
- 如果需要新增未列出的跨层抽象、改变已确认语义或扩大交易所范围，先停止并确认。

## 本轮目标

这轮优化不改变 Poise 的核心产品方向。曲线仍然负责给出理论库存目标，risk acquisition 仍然用价格优势或在线等待时间逐步释放新增风险，executor 仍然负责追随已经释放的目标。

本轮要解决的是：

1. 修复会直接影响新增风险、交易事实和恢复正确性的缺陷。
2. 让同一目标在 reconciler、executor、read model 和 replay 中只有一套解释。
3. 让系统直接说明当前四层目标和为什么没有产生新 submit。
4. 保持 fresh-session 边界，不恢复陈旧 runtime 状态。
5. 用真实决策链回放验证 delayed acquisition 的成本收益，而不是只看曲线。

## 已确认设计

### 决策链

统一使用下面的语义链：

~~~text
policy_target
  -> risk_approved_target
  -> acquisition_frontier
  -> execution_target
  -> execution constraints
  -> order plan
~~~

归属固定为：

- core::strategy 只拥有 policy target。
- engine::reconciler 唯一拥有 policy target 到 execution target 的计算。
- risk_exposure_gate 只拥有当前 session 的 frontier、anchor 和 release 节奏。
- executor 只接收 risk-approved target 和 final execution target，不读取 frontier，也不重新计算 backlog。
- manager 把 reconciler 与 executor 的结果组合成一次 committed decision。
- read model、server projector、protocol 和 TUI 只投影 committed decision，不重新运行状态机。
- decision replay 复用真实 manager、reconciler 和 executor 路径，不复制策略公式。

### Fresh-session

进程重启代表开启新的 acquisition session：

- 启动先清理 inherited open orders。
- 当前仓位、订单、余额、容量、规则和行情从交易所重新查询。
- 旧 bindings、boundary progress、price gate、recovery anomaly、frontier、anchor 和 stale timer 全部作废。
- 不持久化 risk acquisition checkpoint，不恢复旧 effect，不把 effect journal 升级成 outbox。
- 离线时间不计入 stale wait。

本轮选择保守重启语义：

- fresh session 第一笔有效行情只允许降低当前风险。
- 如果当前 risk-approved target 比交易所当前仓位风险更高，acquisition frontier 从当前真实仓位开始，不因为进程重启授予新增风险。
- 当前目标要求减仓时，立即允许减到 risk-approved target。
- 当前目标跨零时，先允许平到 0，新方向重新等待 advantage 或 stale。
- initial_ratio 不用于进程重启的第一笔 acquisition；它仍属于同一运行 session 内新 acquisition episode 的释放规则。

这个选择的目标不是让重启轨迹等同连续运行，而是保证重启本身不成为增加风险的原因。

### 持久化准入

一份状态只有同时满足以下条件，才允许持久化：

1. 当前配置和交易所事实无法可靠重建。
2. 经过进程死亡和时间流逝后语义仍然成立。
3. 丢失它比恢复陈旧数据更危险。
4. 有明确的身份、版本和失效规则。

因此本轮只保留或补强：

- TrackControlState：用户和风控的 durable 控制意图。
- TrackPnlRecord：可对账的成交、费用和资金费历史。
- account monitor baseline：同一交易日的风险参照。
- track_id 到 venue/symbol 的最小身份绑定。

明确不持久化：

- 当前仓位、挂单、账户摘要、交易规则和行情。
- desired/execution target、bindings、boundary progress、price gate 和 recovery anomaly。
- risk acquisition frontier、anchor 和 stale timer。
- 当前 session effect queue。

track events 和 track effects 只用于审计与诊断，不参与启动恢复或订单重放。

### 概念预算

本轮允许新增：

- track identity 持久表：本地历史归属事实。
- RecentTrackPnlRecords：端口返回契约，只区分 Available 与 Unsupported。
- exchange observation revision：私有运行时并发有效性字段。
- current-session submit 授权生命周期：私有运行时协议，保证已经越过 dispatch 边界的订单身份继续被 binding 和 effect queue 拥有。
- SubmitOutcomeUnknown：ExecutionPort 错误契约，只表示 submit 可能已被交易所接受但本地没有权威结果。
- InventoryDecisionSnapshot：当前 session 的只读 committed decision。
- DecisionReason：非持久化诊断枚举。
- fresh acquisition pending marker：fresh start 后只存在于 runtime 的私有标记。
- DecisionReplayInput / DecisionReplayReport：engine 的纯评估 API。
- readiness response/blocker：protocol 契约，不持久化。

本轮不新增：

- runtime snapshot、risk acquisition checkpoint 或 warm-restart cache。
- durable outbox、effect replay worker、actor 或 event sourcing。
- submit effect 自动重派，或跨进程恢复旧 session 的 submit 生命周期。
- 通用 capability registry。
- 每层 target 对应的一组 DTO、service 或 adapter。
- strategy plugin 框架。
- 完整撮合引擎。

## Goal 总览

| 顺序 | Goal | 优先级 | 难度 | 状态 | 依赖 |
| --- | --- | --- | --- | --- | --- |
| 0 | 建立正确性与交易事实底线 | P0 | L | in_progress | 无 |
| 1 | 统一决策 owner 与可解释性 | P1 | L | pending | Goal 0 |
| 2 | 建立真实 decision replay 与 Workbench 评估 | P2 | L | pending | Goal 1 |
| 3 | 完成整体验收与长期文档同步 | P1 | M | pending | Goal 0 到 Goal 2 |

## Goal 0. 建立正确性与交易事实底线

目标：先修复会直接影响新增风险、历史归属、交易事实和恢复顺序的问题。

Task 清单：

| Task | 状态 | 验收摘要 | Commit |
| --- | --- | --- | --- |
| 0.1 修复跨零新增风险计算 | completed | 反方向目标只把平仓后的新方向仓位计为新增风险 | 8c10ece |
| 0.2 收紧配置数值与默认值契约 | completed | Rust、server、Workbench 对非法值和默认值结论一致 | de69e29 |
| 0.3 建立 track identity guard | completed | track_id 不能把旧控制或 PNL 重新解释成另一 instrument | 903ab2c |
| 0.4 消除 PNL backfill 空成功并补齐当前实用 venue | completed | Available(empty)、Unsupported、transient error 明确分离 | 9f35f74 |
| 0.5 修复 REST 旧快照覆盖 WS 新事实 | completed | observation revision 改变时旧 REST 快照不能应用 | f0fcc4a |
| 0.6 增加 trading readiness 并校正文档中的 journal 语义 | completed | readiness 区分 blocker 与 warning，journal 不参与恢复 | e2708bd |
| 0.7 固化 protected-state 订单维护与 current-session submit 授权生命周期 | completed | 已授权 submit 在 receipt、order fact 或完整快照裁决前不丢失订单身份 | a6ee855 |
| 0.8 区分 submit final failure 与 outcome unknown | in_progress | 结果未知时先保留所有权并完整对账，不盲目提交替代订单 | — |

### Task 0.1 修复跨零新增风险计算

改动范围：

- engine/src/execution_gate.rs
- engine/src/reconciler.rs

验收：

- current +3 到 target -0.5，需要 0.5 个新空头单位。
- current -3 到 target +0.5，对称处理。
- 同方向减仓、反方向平到 0、目标归零不消耗新增风险容量。
- current 0 到正负目标按目标绝对值消耗容量。
- 容量刚好时允许，少于所需值时阻止。

实现约束：

- 新增风险计算只能有一个 owner，优先放在 execution_gate 内部的私有函数。
- executor 不复制跨零分解规则。

最小验证：

~~~bash
cargo test -p poise-engine execution_gate::tests::
cargo test -p poise-engine reconciler::tests::
git diff --check
~~~

### Task 0.2 收紧配置数值与默认值契约

改动范围：

- core/src/strategy.rs
- core/src/risk.rs
- server/src/config.rs
- tools/track-tuning-workbench/src/domain/
- README.md

验收：

- lower_price、upper_price 有限且为正，lower 严格小于 upper。
- long/short capacity 有限、非负，且不能同时为零。
- notional_per_unit、max_notional 和 loss limits 有限且满足各自正值约束。
- min_rebalance_units 有限且严格大于零。
- risk acquisition 参数继续满足当前比例、步长和 stale 约束。
- core、server 和 Workbench 对相同非法输入给出相同结论。
- README 的 initial_ratio 默认值与 core 当前默认 0.5 一致。

实现约束：

- 本 task 不拆 min_rebalance_units；它暂时继续作为统一 exposure quantum。
- 不为 Rust/TypeScript 配置生成引入新框架，Task 2.3 再删除 Workbench 中重复决策公式。

最小验证：

~~~bash
cargo test -p poise-core strategy::tests::
cargo test -p poise-core risk::tests::
cargo test -p poise-server config::tests::
(cd tools/track-tuning-workbench && pnpm test)
git diff --check
~~~

### Task 0.3 建立 track identity guard

改动范围：

- application/src/track_definition.rs
- application 持久化 port
- storage/src/schema.rs
- storage/src/sqlite.rs
- server/src/state_bootstrap.rs

持久化内容只允许是：

~~~text
track_id
venue
symbol
bound_at
~~~

验收：

- 空数据库首次启动绑定当前 track_id 与 Instrument。
- 已绑定且匹配时正常启动。
- 相同 track_id 改成其他 venue 或 symbol 时，在连接交易所或修改交易所状态前失败。
- 配置删除旧 track 时保留历史绑定；以后不能用同一 ID 指向另一 instrument。
- legacy PNL 只有一个唯一 instrument 且与当前配置匹配时，自动补建绑定。
- legacy PNL 存在多个 instrument 或与当前配置冲突时，拒绝启动。
- 只有 control/events、没有 instrument 证据时，首次升级采用当前配置并记录一次明确的 legacy adoption 日志。
- 写入 PNL 时再次验证 record instrument 与绑定一致，失败时不写入。

实现约束：

- 新增独立小表即可，不引入通用 migration service。
- 不保存完整 TrackConfig、quantity kind、settlement asset、exchange metadata 或 runtime 状态。

最小验证：

~~~bash
cargo test -p poise-storage track_identity
cargo test -p poise-server state_bootstrap::tests::
git diff --check
~~~

### Task 0.4 消除 PNL backfill 空成功并补齐当前实用 venue

这个 task 是一次端到端共享接口迁移，不能拆成 trait、adapter、health 三个半成品提交。

改动范围：

- engine/src/ports.rs
- exchanges/binance/
- exchanges/bybit/
- exchanges/hyperliquid/
- exchanges/okx/
- server/src/runtime/pnl_backfill.rs
- server/src/runtime/pnl_startup_audit.rs
- server/src/pnl_audit.rs
- server/src/http.rs
- protocol/src/lib.rs

端口返回：

~~~text
RecentTrackPnlRecords::Available(records)
RecentTrackPnlRecords::Unsupported
~~~

验收：

- Available(empty) 只表示支持查询但没有记录。
- Unsupported 不得再被记录为成功。
- transient API error 与 Unsupported 使用不同状态和错误路径。
- 每个 exchange adapter 必须显式实现，trait 不提供空数组默认值。
- OKX 保持 Available。
- Hyperliquid 补齐能稳定映射的 recent fills、fee、realized PNL 和 funding；完成前保持 Unsupported。
- Binance、Bybit 若本轮不补齐，显式 Unsupported，并在启动阶段拒绝进入交易 runtime。
- 任何必需静态能力为 Unsupported 时，本 task 就必须启动失败，不能等到后续 readiness 才处理。
- backfill、startup audit、recent-fills audit、HTTP 和健康投影对三种结果结论一致。
- source key 保持幂等，instrument identity 在写入前通过 Task 0.3 校验。

实现停点：

- 执行前从当前实例配置重新确认实际运行 venue，只读取非敏感 venue/symbol 信息。
- 如果 Hyperliquid 历史 API 无法稳定映射 TrackPnlRecord 或 funding 无法可靠归属，保持 Unsupported 并停止，不创造临时 PNL 表或猜测归属。
- 不引入通用 capability registry。

最小验证：

~~~bash
cargo test -p poise-server pnl_backfill::tests::
cargo test -p poise-server pnl_startup_audit::tests::
cargo test -p poise-server pnl_audit::tests::
cargo test -p poise-server http::tests::
cargo test -p poise-okx recent_track_pnl_records
cargo test -p poise-hyperliquid
cargo check -p poise-binance -p poise-bybit -p poise-hyperliquid -p poise-okx
git diff --check
~~~

### Task 0.5 修复 REST 旧快照覆盖 WS 新事实

改动范围：

- engine/src/runtime.rs
- engine/src/mutation_frame.rs
- engine/src/manager.rs
- application/src/mutation_executor.rs
- server/src/runtime/reconcile.rs
- server/src/runtime/exchange_state.rs

验收：

- REST 请求阻塞期间注入较新的 WS position update，旧 REST position 不得覆盖。
- order update 同样成立。
- revision 未变化时，REST position/open-orders 快照正常应用。
- revision 变化后，旧快照被丢弃或重试，最终能够收敛。
- revision 检查与快照应用在现有 per-track mutation owner 内原子完成。
- 不跨 REST 网络请求持锁。
- PNL 和 market data 不属于 position/open-orders 快照，不无意义推进该 revision。

实现约束：

- revision 覆盖所有会改变 position、open orders 或本地 binding 解释的 exchange observation。
- ExchangeFreshness 继续只表示 unknown/stale，不承担正常 observation 顺序。
- 如果实现只能在 server 外部先比较、随后再调用 application mutation，停止；这仍然有竞态。

最小验证：

~~~bash
cargo test -p poise-server runtime::reconcile::tests::
cargo test -p poise-server runtime::exchange_state::tests::
cargo test -p poise-application mutation_executor::tests::
git diff --check
~~~

### Task 0.6 增加 trading readiness 并校正文档中的 journal 语义

改动范围：

- server/src/http.rs
- server/src/runtime/health.rs
- application diagnostics
- protocol/src/lib.rs
- scripts/probe-health.sh
- docs/system-overview.md

验收：

- 保留现有 health 入口的进程与 task 观测用途。
- 新增明确的 trading readiness 响应，包含 ready_to_trade、blockers 和 warnings。
- Unknown 的关键交易 task 阻塞 readiness。
- market data、user data、account monitor、effect worker、recovery、PNL capability/backfill 的关键异常进入 blockers。
- effect journal 写入失败进入 warning/diagnostics，但不恢复旧 effect、不回滚已经提交的业务状态，也不升级成 outbox。
- readiness 不持久化，不重新运行 engine 状态机。
- probe 脚本明确选择检查 health 还是 readiness，不再把任意 HTTP 200 当作同一种成功。

实现约束：

- 如果为了 readiness 开始引入通用状态总线或数据库表，停止。
- Unsupported 的必需静态能力应在启动阶段失败；readiness 不能成为系统继续不安全交易的借口。

最小验证：

~~~bash
cargo test -p poise-server http::tests::
cargo test -p poise-server runtime::health::tests::
cargo test -p poise-protocol
git diff --check
~~~

### Task 0.7 固化 protected-state 订单维护与 current-session submit 授权生命周期

改动范围：

- engine/src/executor/
- engine/src/manager.rs
- application/src/submit_effect_service.rs
- application/src/session_effect_queue.rs
- application/src/mutation_executor.rs
- application/src/read_model.rs
- server/src/effect_worker/

验收：

- submit dispatch 前进入保护态时，pending binding 被 supersede，且不调用交易所。
- submit 已越过 dispatch 边界后进入保护态时，binding 和 current-session effect queue 继续拥有同一 client order id。
- receipt、WS order fact 或完整 open-orders 快照找到该订单后，若已不再需要，恰好产生一次 cancel。
- receipt 与保护态并发时，不出现 binding 已删除但真实订单无人管理的窗口。
- Frozen 与 FlattenPending 只做 maintenance：撤销风险增加订单，保留方向和数量仍安全的 reduce-only maker，且不产生新 submit。
- Flattening 与 Terminated 继续完整 ReduceOnly，不保留普通 maker。
- 异常或不完整的交易所快照不部分消费 binding，也不释放 queue ownership。
- terminal order fact 对后到的旧 New/PartiallyFilled fact 保持单调，不复活 binding。

实现约束：

- 生命周期只存在于当前进程，复用现有 binding、session effect queue 和 exchange sync owner。
- 不新增 durable outbox、effect replay、数据库表或跨进程恢复。
- 不在 read model、server 或 exchange adapter 复制生命周期决策。

最小验证：

~~~bash
cargo test -p poise-engine executor::
cargo test -p poise-engine manager::tests::
cargo test -p poise-application submit_effect_service::tests::
cargo test -p poise-application mutation_executor::tests::
cargo test -p poise-server effect_worker::tests::
git diff --check
~~~

### Task 0.8 区分 submit final failure 与 outcome unknown

改动范围：

- engine/src/ports.rs
- engine/src/mutation_frame.rs
- application/src/mutation_executor.rs
- application/src/session_effect_queue.rs
- application/src/submit_effect_service.rs
- server/src/order_outcome.rs
- server/src/effect_worker/
- exchanges/binance/
- exchanges/bybit/
- exchanges/hyperliquid/
- exchanges/okx/

验收：

- 明确的交易所拒单、余额不足、非法价格精度和确定的限频响应继续作为 final failure。
- metadata、序列化、签名、请求构建和明确的 connect failure 等发送前错误继续作为 final failure。
- 请求可能已经越过发送边界，但发生 timeout、连接中断、响应体读取失败或成功响应无法解析时，adapter 返回 typed SubmitOutcomeUnknown。
- SubmitOutcomeUnknown 不 terminalize binding，不把 effect 记为 failed/superseded，也不重新 dispatch 同一仓位缺口。
- worker 标记 exchange state stale，触发完整 position/open-orders sync，并把当前 submit effect deferred 到 ExchangeState。
- receipt、WS order fact 或完整快照找到同一 client order id 时，复用 Task 0.7 生命周期吸收事实。
- submit 返回 unknown 后的第一次成功同步仍按 DispatchInFlight 处理：它可以吸收已出现的订单事实，但空结果不能证明请求不会稍后落地；同步失败时不得消耗这个确认阶段。
- 只有切换到 AwaitingExchangeState 后，后续独立且无 anomaly 的完整快照再次确认订单不存在，才能结束旧 submit effect 并允许重新规划。
- absence confirmation 按 open-orders 再 position 的顺序读取；订单若在两个查询之间成交，较后的 position 必须反映成交，不能组成“旧仓位 + 空挂单”的错误缺席证据。
- 对账失败、revision 变化或 recovery anomaly 时继续 deferred，不释放 ownership。
- fresh-session 边界不变：进程重启仍从交易所重建，不恢复旧 queue 或 binding。

实现约束：

- 由 exchange adapter 拥有“请求是否可能已发送”的知识；server 不解析错误字符串猜测结果。
- submit 不使用会跨 transport、5xx、429 或 307/308 redirect 自动重派的通用 HTTP 行为；一次 dispatch 只发送一次，后续依靠稳定 client order id 和完整交易所对账。
- submit 与 cancel 使用 operation-specific error mapping，不能让 cancel-only 错误种类泄漏到 submit。
- InFlight 到 AwaitingExchangeState 的切换与已吸收 order fact 的补查必须在同一 track mutation 临界区完成，避免 WS 事实早于网络错误返回时遗留旧 owner。
- 只新增 ExecutionPortErrorKind::SubmitOutcomeUnknown，不新增 submit service、DTO 或持久化状态。
- 不自动重派；是否需要新订单由完整对账后的正常 reconcile 决定。

最小验证：

~~~bash
cargo test -p poise-engine ports
cargo test -p poise-application session_effect_queue::tests::
cargo test -p poise-server order_outcome::tests::
cargo test -p poise-server effect_worker::tests::
cargo test -p poise-binance
cargo test -p poise-bybit
cargo test -p poise-hyperliquid
cargo test -p poise-okx
git diff --check
~~~

## Goal 1. 统一决策 owner 与可解释性

目标：让同一次库存决策只计算一次、只提交一次，并能直接解释为什么没有新 submit。

Task 清单：

| Task | 状态 | 验收摘要 | Commit |
| --- | --- | --- | --- |
| 1.1 让 executor 只消费 final execution target | completed | executor 不再读取 frontier 或重复计算 backlog target | 89f6761 |
| 1.2 建立 committed InventoryDecisionSnapshot | pending | 查询只投影已提交决策，不推进 gate 或 timer；依赖 Task 0.8 | — |
| 1.3 统一公开词汇并增加 DecisionReason | pending | API、WebSocket、TUI 使用同一 revision 和原因 | — |
| 1.4 固定保守 fresh-session acquisition 语义 | pending | 重启本身不授予新增风险，离线时间不触发 stale | — |

### Task 1.1 让 executor 只消费 final execution target

改动范围：

- engine/src/reconciler.rs
- engine/src/manager.rs
- engine/src/executor/planning.rs
- engine/src/executor/policy.rs

验收：

- reconciler 计算 policy、risk-approved、frontier 和 execution target。
- executor 输入删除 risk_release_frontier。
- CatchUp 只追 final execution target。
- reduce-only maker 仍可使用 risk-approved target 判断降低风险方向。
- 风险降低不被 frontier 阻塞。
- 跨零时 executor 不自行越过已经给出的 execution target。

实现约束：

- 本 task 先消除重复 owner，不同时做公开协议大改名。
- 不新增 PolicyDecision、RiskApproval 等只包装一个 Exposure 的浅 DTO。

最小验证：

~~~bash
cargo test -p poise-engine reconciler::tests::
cargo test -p poise-engine executor::planning::tests::
cargo test -p poise-engine manager::tests::
git diff --check
~~~

### Task 1.2 建立 committed InventoryDecisionSnapshot

改动范围：

- engine/src/runtime.rs
- engine/src/mutation_frame.rs
- engine/src/manager.rs
- application read source

snapshot 至少包含：

~~~text
observed_at
policy_target
risk_approved_target
acquisition_frontier
execution_target
applied_risk_cap
price_execution_gate
decision revision
~~~

验收：

- manager 完成一次 reconcile/planning 后更新 snapshot。
- snapshot 进入 mutation frame，持久化失败回滚时一起回滚。
- snapshot 是 current-session 只读事实，不写 SQLite。
- fresh start 清空旧 snapshot。
- 连续查询同一 revision，不改变 target、frontier、timer、events 或 effects。
- runtime query 不再用 Utc::now() 重跑 reconciler。
- price gate 直接投影真实滞回状态，不从 Open 重新计算。
- 纯 snapshot/diagnostic 变化不触发 durable write。

最小验证：

~~~bash
cargo test -p poise-engine runtime::tests::
cargo test -p poise-engine manager::tests::
cargo test -p poise-application
git diff --check
~~~

### Task 1.3 统一公开词汇并增加 DecisionReason

改动范围：

- engine/src/execution_plan.rs
- engine/src/executor/
- engine/src/reconciler.rs
- engine/src/manager.rs
- application/src/read_model.rs
- protocol/src/lib.rs
- server/src/projector.rs
- server/src/websocket.rs
- tui/

统一公开词汇：

~~~text
policy_target
risk_approved_target
acquisition_frontier
execution_target
~~~

最小 reason 集：

~~~text
waiting_market_data
paused
at_execution_target
waiting_risk_release
account_capacity_blocked
price_gate_blocked
below_rebalance
below_venue_minimum
covered_by_working_order
recovery_required
waiting_cancel
submit_planned
~~~

验收：

- policy target 在 risk cap 场景下仍保留原始曲线值，不被 approved target 覆盖。
- reconciler 和 executor 在各自知识边界提供 reason，manager 选择最终主因。
- DecisionReason 只用于诊断，不驱动控制流、不持久化、不生成每 tick domain event。
- 删除 TrackEffect::NoOp，空 effects 表示没有副作用。
- 有 reason 不代表跳过订单维护；例如 price gate blocked 时仍可产生必要 cancel。
- HTTP、WebSocket 和 TUI 展示同一个 decision revision、四层 target 和 reason。
- 探索阶段不保留长期双字段兼容层，所有直接消费者在同一 task 内更新。

最小验证：

~~~bash
cargo test -p poise-engine
cargo test -p poise-application read_model::tests::
cargo test -p poise-server projector::tests::
cargo test -p poise-server websocket::tests::
cargo test -p poise-protocol
cargo test -p poise-tui
git diff --check
~~~

### Task 1.4 固定保守 fresh-session acquisition 语义

改动范围：

- engine/src/runtime.rs
- engine/src/risk_exposure_gate.rs
- engine/src/reconciler.rs
- server/src/runtime/startup_bootstrap.rs
- application read model
- protocol/src/lib.rs
- tui/

验收矩阵：

| 场景 | fresh-session 第一笔 execution target |
| --- | ---: |
| current +2，approved +8 | +2，不因重启增仓 |
| current +8，approved +2 | +2，立即降低风险 |
| current +2，approved -4 | 0，只平旧方向 |
| current 0，approved +8 | 0，等待 advantage 或 stale |
| 停机一个月后启动 | stale 从新行情时刻开始 |
| 旧 journal 有 pending effect | 不重发 |
| 旧 runtime 曾有 frontier | 完全忽略 |

可观察字段至少包含：

~~~text
acquisition_reset_reason = fresh_session_start
acquisition_session_started_at
initial_frontier
initial_release_units
~~~

实现约束：

- 使用 current-session 私有状态表达 fresh acquisition pending，不新增表。
- 不保存 frontier、anchor、stale timer 或旧 target。
- inherited orders 仍按现有 startup cleanup 清理。
- 如果实现需要区分首次启用和进程重启并引入 durable activation intent，先停止确认；本 task 默认所有 fresh process session 都采用保守规则。

最小验证：

~~~bash
cargo test -p poise-engine risk_exposure_gate::tests::
cargo test -p poise-engine reconciler::tests::
cargo test -p poise-engine runtime::tests::
cargo test -p poise-server runtime::startup_bootstrap::tests::
cargo test -p poise-protocol
cargo test -p poise-tui
git diff --check
~~~

## Goal 2. 建立真实 decision replay 与 Workbench 评估

目标：用真实 engine 决策路径回答 delayed acquisition 是否改善成本，以及它付出了多少跟踪误差和机会成本。

Task 清单：

| Task | 状态 | 验收摘要 | Commit |
| --- | --- | --- | --- |
| 2.1 建立确定性 decision scenario runner | pending | 时间戳输入通过真实 manager 路径产生稳定报告 | — |
| 2.2 增加黄金场景和 Immediate/Delayed 指标 | pending | backlog、restart、cross-zero、fill model 可量化比较 | — |
| 2.3 Workbench 改用 Rust decision evaluator | pending | risk acquisition 参数真实改变指标，不再只改变 TOML | — |

### Task 2.1 建立确定性 decision scenario runner

改动范围：

- engine 新增聚焦的 decision replay 模块
- core/src/replay.rs
- 必要的测试 fixture

输入：

- TrackDefinition 与 ExchangeRules。
- 带时间戳的 market、position、order observation。
- 显式 Restart 步骤。
- 注入 clock。
- 显式 fill model。

第一版只支持：

~~~text
ImmediateFullAtOrderPrice
NeverFill
~~~

输出：

- 每一步 committed InventoryDecisionSnapshot。
- 语义化 effects。
- 模拟仓位和费用。
- 汇总 DecisionReplayReport。

验收：

- 每一步调用真实 TrackManager 路径。
- Restart 调用真实 fresh-start 构建逻辑。
- 相同输入和 clock 重跑得到稳定报告。
- 规范化随机 client order id，不用随机字段破坏比较。
- 现有 core replay 明确标记为 curve-immediate benchmark，不再被描述为完整策略 replay。

实现约束：

- 不直接复用 reconciler 私有函数绕过 manager。
- 不模拟交易所排队位置、成交概率、延迟或滑点。

最小验证：

~~~bash
cargo test -p poise-engine decision_replay
cargo test -p poise-core replay::tests::
git diff --check
~~~

### Task 2.2 增加黄金场景和 Immediate/Delayed 指标

首批场景：

1. 单调有利行情：advantage 精确释放。
2. 行情反向：显示 delayed 的跟踪误差和机会成本。
3. stale：阈值前不释放，在线等待达到阈值时释放。
4. 跨零：先平仓，再等待新方向。
5. restart：旧 frontier 丢弃，第一笔不新增风险。
6. 长时间离线 restart：离线时间不触发 stale。
7. NeverFill：frontier 与 position 的差异明确可见。
8. partial/current-position observation：交易所事实更新后重新决策。
9. price gate 滞回：决策与 committed view 一致。
10. inverse contract：数量、notional、margin 和 fee 使用 ExchangeRules。

指标：

- time-weighted tracking error。
- max/mean backlog。
- authorized-but-unfilled gap。
- release count。
- max single catch-up units。
- long/short risk-increase weighted fill price。
- turnover notional。
- estimated fees。
- max notional 和 margin。
- restart sensitivity。

验收：

- 同一场景能并列输出 curve-immediate 与 delayed decision 结果。
- 报告明确区分价格改善和仓位跟踪损失。
- 不把简单 fill model 的结果描述为真实成交预测。

最小验证：

~~~bash
cargo test -p poise-engine decision_replay
cargo test -p poise-core replay::tests::
git diff --check
~~~

### Task 2.3 Workbench 改用 Rust decision evaluator

改动范围：

- tools/track-tuning-workbench/src-tauri/
- tools/track-tuning-workbench/src/domain/
- tools/track-tuning-workbench/src/app/
- tools/track-tuning-workbench/src/ui/

验收：

- Tauri 调用 poise-engine 的 decision replay API。
- Rust 负责配置、ExchangeRules、曲线、risk gate、decision replay 和指标。
- TypeScript 只保留表单即时校验、展示和图表交互。
- 删除 TypeScript 中重复的曲线、数量和策略指标公式。
- initial_ratio、advantage_steps、release steps、catchup_ratio、stale 任一参数在对应场景下都能改变报告。
- inverse fixture 与 Rust ExchangeRules 结果一致。
- Workbench 展示四层 target、DecisionReason 和 decision revision。
- curve-immediate benchmark 与 delayed decision replay 并列展示。

最小验证：

~~~bash
cargo test -p poise-track-tuning-workbench
(cd tools/track-tuning-workbench && pnpm test)
(cd tools/track-tuning-workbench && pnpm build)
git diff --check
~~~

## Goal 3. 完成整体验收与长期文档同步

目标：确认本轮形成完整闭环，删除已经失效的旧叙述，不保留阶段性双重事实源。

Task 清单：

| Task | 状态 | 验收摘要 | Commit |
| --- | --- | --- | --- |
| 3.1 最终验收并同步长期文档 | pending | workspace、Workbench 和长期文档全部通过，已完成任务被清理 | — |

### Task 3.1 最终验收并同步长期文档

改动范围：

- README.md
- docs/system-overview.md
- docs/next-goals.md

功能验收：

- 跨零容量计算正确。
- track identity 不匹配在任何交易所修改前失败。
- PNL capability 不再空成功，当前实用 venue 有可对账路径。
- 新 WS 事实不能被旧 REST 快照覆盖。
- 重启不恢复旧 runtime，也不因第一笔行情授予新增风险。
- 同一 committed revision 重复查询不会改变 target、frontier 或 timer。
- API/TUI 能说明四层 target 和没有新 submit 的主因。
- risk acquisition 参数能够改变 decision replay 和 Workbench 指标。

验证命令：

~~~bash
cargo test --workspace
cargo build -p poise-server
(cd tools/track-tuning-workbench && pnpm test)
(cd tools/track-tuning-workbench && pnpm build)
git diff --check
git status --short
~~~

文档验收：

- system-overview 记录最终 decision ownership、fresh-session、持久化、readiness、replay 和 effect journal 语义。
- README 默认值、公开路由和工具说明与实现一致。
- track_effects 明确是诊断 journal，不再描述为持久派发队列。
- core replay 明确是 curve-immediate benchmark。
- 本文已完成 goal 的长期信息吸收后，删除对应完成项和 commit 过程记录。
- 不新增历史 spec、review 或第二份长期架构文档。

文档验证：

~~~bash
rg -n "policy_target|risk_approved_target|acquisition_frontier|execution_target|fresh-session|track_effects|decision replay" README.md docs
git diff --check
~~~

只有本 task 才默认执行 workspace 级 Rust 验收。所有验收和文档更新完成后再提交本 task，并回写 commit SHA。

## 本轮非目标

- 不增加新策略、更多 shape family 或新的 risk acquisition 参数。
- 不实现多 track 账户预算分配。
- 不新增交易所；只补齐当前已宣称能力的事实边界。
- 不持久化 runtime、risk gate、订单、bindings 或当前账户快照。
- 不实现 warm restart 或 checkpoint TTL。
- 不把 effect journal 升级成可靠 outbox。
- 不实现完整撮合、订单簿排队、滑点或成交概率模型。
- 不为了整洁增加浅 service、DTO、adapter 或通用框架。

## 全轮停止条件

出现以下任一情况，停止当前 task 并先确认：

- 工作区出现与当前 task 无关的脏状态或冲突。
- 需要改变本计划已经确认的 fresh-session、持久化或 target ownership。
- 当前交易所 API 无法稳定提供计划所需事实。
- 需要把 Unsupported 伪装成成功才能继续。
- 新抽象主要是转发调用，没有减少重复 owner 或特殊情况。
- 局部测试不足以覆盖真实影响，但扩大验证后出现无法归因的失败。
