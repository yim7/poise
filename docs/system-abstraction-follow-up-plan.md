# 系统抽象复核后的完善计划

本文是临时实现计划，不是长期架构依据。当前长期依据仍是 [system-overview.md](system-overview.md)。只有全部任务验收完成后，才更新长期依据并删除本文。

基线提交：`1bfff36`

## 范围与前提

- Poise 是单机、单进程、单交易所实例应用；不设计分布式锁、跨进程一致性协议、可靠 outbox 或数据库与交易所之间的分布式事务。
- 同一 track 的进程内更新继续由 application 的 mutation guard 串行化；不同 track 可以并行。
- 不兼容历史数据库，不增加迁移、双读、旧字段 fallback 或过渡 adapter。
- 本轮只处理已经由代码调用链和交易所文档确认的问题，不为纯理论风险增加抽象。

## 复核结论

| 原评审项 | 结论 | 原因与处理 |
| --- | --- | --- |
| PNL 插入后、内存安装前调用方被取消 | 条件成立，但不是当前在线正确性缺陷 | `record_track_pnl` 确实在 SQLite `spawn_blocking` 返回前存在 Future 取消窗口；但当前生产调用方只有 user-data task、启动 replay 和 PNL backfill。在线阶段没有 timeout/abort，显式 abort 只发生在 HTTP 已停止、effect worker 已排空之后的进程 shutdown。即使 shutdown 中 SQLite 已落盘而内存未安装，下一次启动也会从 PNL 明细重建统计。当前不增加 PNL 专用 detached owner task。 |
| Position/Order transition 隐式取时 | 部分属实，原修法不准确 | 注入的单机 `ClockPort` 本身不是问题；真正的问题是行情的交易所发生时间同时被当成本地决策评估时间，延迟行情到达时可能短暂被当作新鲜行情。Position/Order 的 `event_time` 也不应直接替代本地评估时间。需要明确区分 occurrence time 与 evaluation time，并合并重复 reconcile 路径。 |
| Bybit live execution PNL 丢失成交字段 | 属实，优先修复 | Bybit execution 消息提供 `side`、`orderId`、`execPrice`、`execQty`、`execTime`，当前只保存 summary，并错误使用 `creationTime`。Bybit recent PNL 又是 `Unsupported`，丢失字段不能依赖回补修复。 |
| `PositionQty` 进入 runtime 后退化为 `f64` | 属实，是类型边界问题 | 当前不会自动造成数值错误，但 state、fresh-session 输入和换算 helper 失去 canonical quantity 语义，容易与 exposure、contracts 或普通数值混用。只修业务状态和换算边界，不把所有展示指标都包装成新类型。 |
| Binance 根据 symbol 猜 base/settlement asset | 属实，优先修复 | REST recent trade 与 WebSocket trade/funding 都存在 suffix 推断，违反 metadata 缺失即失败的既定约束。当前支持的常见 symbol 多数能工作，但事实 owner 错误，扩展 symbol 或保证金币种时会静默写错历史。 |
| 单 track 更新复制整个 `TrackManager` | 属实，是单机热路径问题 | 每个行情、仓位、订单或 effect 结果都会复制全部 tracks，再只取回一个 `TrackRuntime`。当前 mutator 只访问目标 track，因此暂未形成跨 track 正确性错误，但复杂度和成本随 track 数与行情频率放大，也让未来代码可能无意读取候选 manager 中的其他旧快照。 |
| `AccountCapacitySnapshot` 位于 application 公共 exchange 模块 | 属实，但优先级低 | 它只由 server 启动容量计算和 guard 使用，不是交易所 port 契约。应移回 server 私有模块，不新增替代 DTO。 |

## 确认后的设计标准

### 单机提交语义

`TradingRuntime::handle` 继续是唯一业务更新入口。一次在线更新只需要保证：

1. 同一 track 串行；
2. durable facts 成功落盘后才安装对应 session state；
3. 持久化失败时不发布候选状态或 effects；
4. shutdown 结束在线服务后允许终止内部采集任务，重启从 SQLite durable facts 与交易所事实建立 fresh session。

命令等可能来自可取消 HTTP request 的 durable transition 继续由现有 owned commit task 完成。PNL 只由进程内部 task 写入，当前不把同一机制扩展到 PNL；如果以后 PNL 入口暴露给可取消请求，届时再统一其提交 owner。

### 时间语义

- `occurred_at`：外部事实发生时间，由交易所消息或行情 tick 提供；用于历史记录、排序和行情年龄起点。
- `evaluated_at`：本机状态机处理本次输入的时间，由注入的 `ClockPort` 在一次 manager 更新开始时采样一次；用于 stale 判断、风险释放和 decision timestamp。
- 延迟到达的行情必须以 `evaluated_at - occurred_at` 判断新鲜度，不能因刚被处理就视为新鲜。
- Position/Order 的交易所 `event_time` 仍用于启动 replay 排序；在线 reconcile 使用本机 `evaluated_at`，不把事件发生时间冒充评估时间。
- engine 保留一个 reconcile 规划路径；不同输入只决定如何更新 observation，以及 transition 的 commit intent，不复制整段 freshness、pause 和 execution planning 逻辑。

### 交易所 metadata 与 PNL

- 每个 adapter 私有维护当前配置 instrument 的 metadata registry；不把 registry、wire metadata 或 symbol 规则提升到共享 port。
- `MetadataPort::get_exchange_info` 成功时写入同一 adapter 内 REST、WebSocket 和 recent-PNL mapper 共享的 registry。
- 当前实例的全部 metadata 在 `ServerRuntime::start` 前已由 assembly 加载，因此 user-data 订阅不做临时网络查询；registry 缺项直接报错。
- `InstrumentSpec` 继续是 canonical quantity、base/quote/settlement 和 contract math 的事实源；交易所实际 cashflow 的 value/asset 仍以成交或账单字段为准。
- 历史 `TrackPnlRecord` 在创建时就带完整 occurrence time、side、price、quantity unit 和 source identity，查询时不使用当前 metadata 重算。

### 单 track 候选状态

- 一次 mutation 只复制目标 `TrackRuntime`，不复制整个 track map 和 instrument index。
- engine 的规划接口从 committed target runtime 生成 result 与候选 runtime；application 只负责 guard、durable commit、安装和通知。
- `exchange_observation_revision` 继续保持现有语义：即使业务候选提交失败，已经到达的 exchange observation 也必须使旧 REST snapshot 失效。
- 不为此新增 `TrackService`、`MutationContext`、通用 observation enum 或第二份 runtime registry。

## 实现任务

执行本文前需要用户确认。确认后严格按顺序推进；每个任务先补验收测试，测试通过后单独提交，并把 commit SHA 写回本文，完成该记录提交后再开始下一任务。

### F1 修正 Bybit execution PNL 事实

- [x] 在 Bybit adapter 内建立 REST/WS 共享的私有 instrument metadata registry，并由 `get_exchange_info` 填充。
- [x] 解析 `side`、`orderId`、`execPrice`、`execQty`、`execTime`；使用 `execTime` 作为 `TrackPnlRecord.occurred_at`。
- [x] 使用 registry 中的 `InstrumentSpec` 构造完整 `TrackPnlQuantity`，删除 settlement suffix 推断和 `trade_summary` 路径。
- [x] registry 缺项、字段缺失或单位不匹配时显式失败。
- [x] 验收：mapper 测试证明 `creationTime != execTime` 时保存 `execTime`，并校验 side、price、quantity、base asset、order/trade identity 和 fee asset。
- Commit SHA：`6d05285`

### F2 修正 Binance PNL metadata ownership

- [x] 在 Binance adapter 内建立 REST/WS 共享的私有 instrument metadata registry，并由 `get_exchange_info` 填充。
- [x] REST recent trades、WebSocket trades 和 funding 归属均从 registry 读取 base/settlement 事实，删除 `strip_suffix` 与硬编码 quote suffix 推断。
- [x] WebSocket trade 使用订单成交时间字段作为 PNL occurrence time；event time 只保留为消息时间。
- [x] WebSocket trade 读取消息中的 margin asset，funding 读取 balance asset；registry 用于验证归属和补足 quantity unit。registry 缺项或归属不一致时显式失败，非结算币 fee 保留原始 asset，不能静默猜测或丢弃。
- [x] 验收：覆盖非普通 symbol、event time 与 trade time 不同、非结算币 fee、metadata 缺失四类测试；REST/WS 对同一成交生成相同 quantity unit。
- Commit SHA：`bbd8c36`

### F3 改为单 track 规划并统一时间语义

- [x] engine 从单个 committed `TrackRuntime` 副本规划候选 transition；PNL 统计也只复制目标 runtime；application 删除所有业务更新中的 `manager.clone()` 候选路径。
- [x] 一次 manager 更新只采样一次 `evaluated_at`；行情输入单独保留 `occurred_at`。
- [x] Position、Order、Command、Market 和 exchange sync 共用同一 reconcile 规划核心，删除 `reconcile_track` / `reconcile_market_track` 的重复决策逻辑。
- [x] 保留 LiveOnly/Commit 差异和 observation revision 的单调失效语义。
- [x] 验收：更新一个 track 的规划接口不能读取或修改另一 track；延迟行情不能触发新增风险；manager clock 与 event time 不同时，decision 使用 evaluation time；持久化失败和取消调用方的既有测试继续通过。
- Commit SHA：`b327cef`

### F4 保持 runtime 的 canonical `PositionQty`

- [ ] `FreshSessionExternalInputs.position_qty` 与 `TrackRuntime.current_position_qty` 使用 `PositionQty`。
- [ ] position notional、exposure 和 required-capacity helper 在业务边界接收 `PositionQty`，只在局部算术内部读取数值。
- [ ] `PositionObservation -> TrackRuntime -> fresh session` 不再提前 `.value()`。
- [ ] read model、protocol 和离线统计等明确投影可以继续输出 `f64`，不扩大类型包装范围。
- [ ] 验收：linear 与 inverse fixture 都证明 canonical quantity 不会和 exposure/contracts 混用，现有投影 JSON 数值结构不变。
- Commit SHA：待执行

### F5 删除 server-only 类型的公共暴露

- [ ] 将 `AccountCapacitySnapshot` 移到 server 启动/guard 私有模块，或直接用现有 server 私有结构表达。
- [ ] application exchange port 不再导出该类型，不新增等价 wrapper。
- [ ] 验收：startup capacity 与 insufficient-margin gate 测试保持通过。
- Commit SHA：待执行

### F6 最终验收并更新唯一依据

- [ ] 运行 `cargo fmt --all -- --check`。
- [ ] 先运行各任务直接相关的 crate 测试，再运行 `cargo test --workspace` 做跨 crate 最终验收。
- [ ] 重新审查状态 owner、时间语义、metadata 事实源、单位边界和新增概念数量。
- [ ] 只在实现与验收全部完成后更新 [system-overview.md](system-overview.md)，删除本文，不保留迁移或历史方案说明。
- Commit SHA：待执行

## 明确不做

- 不增加 PNL 专用 outbox、两阶段提交、分布式事务或进程间锁。
- 不为当前只在 shutdown 出现的 PNL Future 取消窗口增加独立 service/actor。
- 不让 server 或 application 解析交易所 symbol；metadata registry 留在各 adapter 内部。
- 不把 adapter registry 抽成 workspace 级公共 crate。
- 不让 `TrackPnlRecord` 在持久化后再依赖当前 `InstrumentSpec` 补单位。
- 不把所有 `f64` 都替换为 newtype；只保护 canonical quantity 的业务状态与换算边界。
- 不保留旧 schema、兼容字段、迁移脚本、双写或 fallback。
