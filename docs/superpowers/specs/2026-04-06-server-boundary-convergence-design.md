# Server Boundary Convergence 设计

基于本轮架构评审，这次设计处理的是同一组边界问题：

- `engine::ports` 同时承载执行、持久化、effect 调度和 read-side 查询契约
- `server` 通过一个过宽的 `ServerState` 把多类运行时知识暴露给所有调用方
- `TrackWriteService` 对外镜像 `TrackManager` 的 mutation 面，应用层没有形成自己的抽象
- `runtime.rs`、`write_service.rs`、`effect_worker.rs`、`manager.rs` 已经出现明显的大文件信号，模块边界和文件边界没有对齐

这不是单个文件或单个 trait 的问题，而是当前 `server`、`engine`、`storage` 三层之间的 owner 放置不稳定，导致 change amplification、cognitive load 和 unknown unknowns 一起上升。

相关背景：

- 架构评审见 [`../../2026-03-30-architecture-review.md`](../../2026-03-30-architecture-review.md)
- 现有写侧边界设计见 [`2026-03-25-grid-write-boundary-convergence-design.md`](2026-03-25-grid-write-boundary-convergence-design.md)
- 现有运行时边界设计见 [`2026-03-25-grid-runtime-boundary-redesign.md`](2026-03-25-grid-runtime-boundary-redesign.md)
- 现有 application projection 设计见 [`2026-03-26-grid-phase2-application-projection-design.md`](2026-03-26-grid-phase2-application-projection-design.md)

## 1. 目标

- 让 `engine` 只拥有运行态推进知识，不再拥有应用层存取和查询契约
- 让 query、effect 调度、follow-up retirement、通知流回到应用层 owner
- 让写侧服务按 use case 暴露能力，而不是按 `TrackManager` 方法表暴露能力
- 让 `server` 回到 transport 和 runtime host 角色，不再作为应用语义 owner
- 让模块边界和文件边界对齐，降低阅读和安全修改成本

## 2. 非目标

- 这次不引入独立 projection store
- 这次不重写 `TrackManager` 或 `engine` 的状态推进模型
- 这次不修改 HTTP / WebSocket 对外协议语义，只允许必要的类型 owner 调整
- 这次不把 `server` 改成极薄二进制壳
- 这次不处理与边界无关的 UI、交易所 adapter 或策略问题

## 3. 备选方案

### 方案 A：尽量只在 `server` 内整理

做法：

- 保留 `engine::ports` 现状
- 在 `server` 内部拆 `ServerState`
- 继续让 `storage` 实现由 `engine` 定义的 read-side 和 effect queue 契约

优点：

- 改动面较小
- 风险较低

问题：

- query 和 effect orchestration 的 owner 仍然不对
- `engine` 继续被上层变化牵动
- 只是局部整理，不会真正解决 finding

结论：不采用。

### 方案 B：新增 `poise-application`，重新定义 owner

做法：

- 新增 application crate
- 把应用层 query、effect queue、写侧事务、通知流和账户监控迁入该 crate
- 让 `storage` 实现 application-owned store 契约
- 让 `server` 只保留 transport、runtime driver 和 assembly

优点：

- owner 与真实变化原因一致
- `engine` 可以退回纯运行态核心
- 后续拆 context、拆大文件时有稳定边界可依

问题：

- 需要跨 crate 调整类型和依赖方向
- 会带来一次明确的结构迁移

结论：采用。

### 方案 C：同时引入 projection store 和更彻底的运行态重构

做法：

- 在方案 B 基础上，再为 query 引入独立 projection store
- 同时重写更多 runtime 和状态回放逻辑

优点：

- 最终结构可能更独立

问题：

- 这轮主问题是 owner 混乱，不是 projection store 缺失
- scope 明显扩大，容易把边界迁移和新读侧模型混在一起

结论：不作为当前设计。

## 4. 总体结论

采用方案 B：新增 `poise-application`，让 `engine`、`application`、`server`、`storage` 的职责重新对齐。

核心判断：

- `engine` 应只回答“给定当前运行态和输入，状态如何推进”
- `application` 应回答“这些推进结果如何作为应用事务被持久化、查询、调度、通知”
- `server` 应回答“这些应用能力如何被 HTTP、WebSocket、runtime driver 和 worker 消费”
- `storage` 应回答“这些契约在 SQLite 中如何落地”

## 5. Crate 边界

### `poise-core`

职责：

- 纯领域类型和规则

不负责：

- 运行态推进
- 应用层事务
- query
- effect queue
- transport DTO

### `poise-engine`

职责：

- 拥有运行态推进
- 输入是内存中的 `TrackRuntime`
- 输出是 `TrackTransition`、`TrackEffect`、`TrackRuntimeSnapshot`

不再负责：

- 持久化 port
- query port
- effect queue port
- follow-up retirement port

### `poise-application`

职责：

- 拥有应用层写事务、应用层查询、effect 调度、账户监控和通知流

拥有的服务与契约：

- `TrackCommandService`
- `TrackObservationService`
- `TrackEffectService`
- `TrackQueryService`
- `TrackDebugQueryService`
- `AccountMonitor`
- `MutationExecutor`
- `TrackMutationStore`
- `TrackQueryStore`
- `TrackEffectStore`
- `AccountMonitorStore`
- `ApplicationNotification`

拥有的持久化记录类型：

- `PersistedTrackEffect`
- `EffectStatus`
- `EffectStatusUpdate`
- `FollowUpRetirementRequest`
- `StoredTrackEvent`
- `StoredTrackSnapshot`

### `poise-storage`

职责：

- 只实现 application-owned store 契约的 SQLite 落地

不负责：

- 定义 query、effect orchestration 或事务边界

### `poise-server`

职责：

- 只保留 transport、runtime driver 和 assembly

保留模块：

- `http`
- `websocket`
- `projector`
- `account_projector`
- `runtime`
- `effect_worker`
- `assembly`
- `main`

### `poise-protocol`

职责：

- DTO 和 wire contract

## 6. 依赖方向

- `poise-application` 依赖 `poise-core`、`poise-engine`
- `poise-storage` 依赖 `poise-core`、`poise-engine`、`poise-application`
- `poise-server` 依赖 `poise-protocol`、`poise-application`、`poise-storage`、`poise-binance`

这条依赖方向意味着：

- `storage` 可以实现 application-owned store
- `application` 不依赖 `protocol`
- `server` 只消费应用服务和应用模型，不再反向拥有应用语义

## 7. Application 内部边界

### `TrackCommandService`

职责：

- 面向显式用户命令

覆盖的 use case：

- `pause`
- `resume`
- `flatten`
- `terminate`

不负责：

- market / user observations
- effect writeback

### `TrackObservationService`

职责：

- 面向外部事实输入

覆盖的 use case：

- market tick
- position update
- order update
- ledger event
- exchange state sync
- market data freshness refresh

### `TrackEffectService`

职责：

- 面向 side effect 生命周期

覆盖的 use case：

- prepare submit execution
- recover submit effect
- submit receipt writeback
- submit failure writeback
- cancel success writeback
- effect success / failed / superseded
- follow-up retirement 请求与退休

### `TrackQueryService`

职责：

- 从 `TrackQueryStore` 读取快照、最近事件、最近 effects
- 组装 `TrackReadModel`

约束：

- 返回 application model，不返回 protocol DTO

### `TrackDebugQueryService`

职责：

- 基于 query store 和 `TrackReadModel` 产出 diagnostics 所需的应用层结果

### `AccountMonitor`

职责：

- 账户级监控、账户摘要刷新和账户变更通知

### `MutationExecutor`

职责：

- 作为 application 内部深模块，吸收共同写侧复杂度

它拥有：

- per-track mutation lock
- rollback
- 原子提交
- 通知发布
- account margin guard 协调

它不暴露：

- `TrackManager` 的直接接口

### 设计约束

- 三个写侧服务不是 `TrackManager` 的镜像 API
- 服务按调用方意图划分，而不是按 engine 方法名划分
- `TrackManager` 继续作为 engine 内部写侧核心
- application 不再额外引入一层只做转发的 engine facade

## 8. Store 契约

### `TrackMutationStore`

职责：

- 负责一次应用层写事务的原子提交
- 负责加载单个 track 的当前 snapshot

事务内容包括：

- `TrackRuntimeSnapshot`
- 领域事件集合
- 新产生的 `TrackEffect`
- effect status 更新

### `TrackEffectStore`

职责：

- 负责 effect queue 读取和 follow-up retirement 持久化

覆盖能力：

- dispatchable effect 查询
- pending submit 查询
- batch 内 replacement 查询所需数据
- retirement request 的保存、列举和删除

### `TrackQueryStore`

职责：

- 负责只读查询材料

覆盖能力：

- stored snapshot
- recent events
- recent effects

### `AccountMonitorStore`

职责：

- 负责账户监控状态持久化

## 9. 通知与投影

### 通知 owner

通知流归 `poise-application` 拥有。

做法：

- application 服务直接发布 `ApplicationNotification`
- `server` 的 `websocket`、`runtime` 等模块只消费通知

这样可以避免：

- 通知语义继续留在上层 crate
- 应用层为了发通知而依赖向上的宿主模块

### `projector` owner

`projector` 和 `account_projector` 保留在 `poise-server`。

原因：

- `TrackReadModel` 是 application model
- HTTP / WebSocket DTO 是 transport contract
- 两者之间的映射属于 transport adapter，而不是应用逻辑

这保证：

- `poise-application` 不依赖 `poise-protocol`
- 改 DTO 不会反向拖动 application 层

## 10. Server 宿主边界

### `http`

职责：

- 调用 `TrackCommandService`、`TrackQueryService`、`TrackDebugQueryService`
- 使用 `projector`、`account_projector` 映射为 `poise-protocol` DTO

### `websocket`

职责：

- 订阅 `ApplicationNotification`
- 收到变更后重新读取应用层模型
- 使用 `projector`、`account_projector` 映射为 websocket DTO

### `runtime`

职责：

- 驱动 market data、user data、recovery、account refresh 等运行时流程

依赖：

- `TrackObservationService`
- `AccountMonitor`
- `ApplicationNotification`
- 必要时消费 `TrackEffectStore` 的只读能力

不依赖：

- `projector`
- HTTP 状态

### `effect_worker`

职责：

- 执行 submit / cancel side effect

依赖：

- `TrackEffectService`
- `TrackEffectStore`
- `ExchangePort`
- freshness / preflight 等运行时协作者

不依赖：

- query service
- projector
- HTTP 状态

### `assembly`

职责：

- 装配 `poise-application`、`poise-storage`、`poise-binance` 和 `poise-server` 自身模块

约束：

- 只导出按角色组织的小 context
- 不再导出一个全局 `ServerState`

### context 拆分

当前统一 `ServerState` 应改为按角色拆分的上下文，例如：

- `HttpState`
- `WebSocketState`
- `RuntimeState`
- `EffectWorkerState`

拆分原则：

- 每个上下文只暴露当前角色真正需要的能力
- 不用公开大对象来“顺便”共享依赖

## 11. 模块与文件边界

这次设计不只重画 crate owner，也明确约束模块和文件边界。

原则：

- 文件边界应尽量与稳定职责对齐，不按执行顺序或历史演化堆叠
- 顶层文件只保留装配、公开接口和少量编排
- 具体分支逻辑下沉到按子域命名的子模块
- 一个模块应能用一句话说明“它负责什么、依赖什么、隐藏什么”
- 新增一个运行路径或修改一个子域规则时，应尽量只改对应子模块和少量装配代码
- 测试应尽量跟着子域组织，避免所有测试继续堆到单个超大文件末尾
- 不设机械的行数上限；大文件是信号，不是目标
- 如果只是把一个时间线大文件拆成多个顺序文件，不算设计改善

### `server/src/runtime.rs`

目标形态：

- 变成 `server/src/runtime/` 目录
- `mod.rs` 只保留 runtime 组装和主入口
- 细节按稳定职责拆开，例如：启动恢复、market data 消费、user data 消费、reconcile、exchange 同步、账户刷新、guard / preflight

### `server/src/write_service.rs`

目标形态：

- 不再保留一个总写服务文件
- 随 `TrackCommandService`、`TrackObservationService`、`TrackEffectService` 和 `MutationExecutor` 的引入自然消失

### `server/src/effect_worker.rs`

目标形态：

- 变成 `server/src/effect_worker/` 目录
- 按 effect 选择、dispatch、submit / cancel 执行、错误分类与重试、worker loop 拆分

### `engine/src/manager.rs`

原则：

- 不把“文件大”本身当成问题
- 边界迁移后，如果它仍然是单一、内聚的状态推进核心，可以保持较深模块
- 只有在继续混合 command、observation、effect writeback、回放或 guard 等不同变化原因时，才继续做内部子模块拆分

## 12. 迁移原则

- 先调整 owner，再整理实现细节
- 先让依赖方向变干净，再做文件和目录重排
- 不增加新的浅包装层
- 共同复杂度下沉到 `MutationExecutor`
- 保持单一事实源，继续基于快照、事件和 effects 组装读侧

## 13. 推荐迁移顺序

1. 新增 `poise-application`，先放 store contracts、持久化记录类型和通知类型
2. 让 `poise-storage` 实现 application-owned stores，并同步瘦身 `engine::ports`
3. 把 `TrackQueryService`、`TrackDebugQueryService`、`AccountMonitor` 迁入 `poise-application`
4. 把写侧改成 `TrackCommandService`、`TrackObservationService`、`TrackEffectService`，并引入共享 `MutationExecutor`
5. 把 `ServerState` 改成角色化 context
6. 沿着新边界拆 `runtime/`、`effect_worker/` 等目录与测试文件

## 14. 结论

这次设计的重点不是把代码从 `server/` 挪到另一个 crate，而是重新定义 owner：

- `engine` 只拥有运行态推进
- `poise-application` 拥有应用事务、查询、effect 调度、账户监控和通知流
- `poise-server` 只作为 transport 和 runtime host
- `poise-storage` 只负责契约落地

同时，模块与文件边界要跟新的 owner 一起调整。只有 crate 边界变干净、日常阅读和局部修改成本才会真正下降。
