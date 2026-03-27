# 网格平台第二阶段应用投影边界设计

基于第一阶段已经完成的写侧边界收敛，这一阶段只处理上层应用边界，不先进入 `engine` 内部聚合重构。

相关背景：

- 第一阶段写侧边界设计见 [`2026-03-25-grid-write-boundary-convergence-design.md`](2026-03-25-grid-write-boundary-convergence-design.md)
- 运行时边界设计见 [`2026-03-25-grid-runtime-boundary-redesign.md`](2026-03-25-grid-runtime-boundary-redesign.md)
- 当前整体 crate 结构见 [`2026-03-24-grid-platform-architecture-design.md`](2026-03-24-grid-platform-architecture-design.md)

## 1. 背景

第一阶段完成后，系统已经具备这些能力：

- `GridTransition.snapshot + events + effects` 原子提交
- effect 执行从内存动作变成可恢复的持久化 outbox
- `server/runtime` 不再依赖事务外补写维持 effect 恢复语义

但上层应用边界仍然没有收敛：

- `GridPlatformService` 同时承担写侧协调、查询入口、协议 DTO 组装、WebSocket 事件广播
- HTTP / WebSocket 协议仍然贴着内部运行态结构暴露
- `GridSnapshot`、`PendingOrder`、`DomainEvent` 这些内部事实仍被默认当作对外 contract
- TUI 读取到的不是稳定读模型，而是内部 snapshot 的公开版本

这会继续放大三个复杂度症状：

- `change amplification`：改查询、改协议、改推送时都要同时改 `application`、`http`、`websocket`
- `cognitive load`：开发者要记住哪些内部类型可以直接当协议 DTO，哪些不行
- `unknown unknowns`：后续继续调整 `engine` 运行态或 effect 恢复语义时，很难预估会不会把 TUI contract 一起打碎

第一阶段已经解决“写侧提交边界”；第二阶段要解决的是“协议解释权边界”。

## 2. 设计目标

### 2.1 主目标

- 把 `GridPlatformService` 拆成明确的写侧、查询、投影三个应用边界
- 外部协议不再直接暴露 `GridSnapshot`、原始 `PendingOrder`、原始 `DomainEvent`
- HTTP / WebSocket 只暴露面向 TUI 的稳定读模型
- 写侧继续只拥有提交边界，不拥有协议解释权
- 为后续继续重构 `engine` 内部运行态提供缓冲层，避免内部结构变化直接穿透到 TUI

### 2.2 非目标

- 这次不引入完整 CQRS，不做 `CommandBus`
- 这次不引入独立 `ProjectionStore`
- 这次不做 projection replay / rebuild 机制
- 这次不优先服务多个读侧，短期读侧仍只有 TUI
- 这次不处理 `engine` 内部订单生命周期和预算知识的进一步下沉

## 3. 设计问题

第二阶段真正要做的设计决策是：

> 对外协议到底由谁解释内部事实？

第一阶段以前，答案近似于：

- `engine` 给什么 snapshot
- `protocol` 就暴露什么 DTO
- `server` 顺手把它们搬给 HTTP / WebSocket

第二阶段以后，答案应当变成：

- `engine`、`storage` 只提供内部事实
- `server/query + projector` 负责把这些事实解释成稳定读模型
- transport 只搬运读模型结果

## 4. 备选方案

### 方案 A：只拆 `GridPlatformService`，继续保留 snapshot 风格协议

做法：

- 把 `GridPlatformService` 拆成几个文件
- `GET /grids/:id/snapshot` 继续暴露 `GridSnapshot`
- WebSocket 继续广播 `DomainEvent`

优点：

- 改动最小
- transport 改造成本低

问题：

- 只解决文件职责过厚，不解决协议解释权外泄
- `protocol` 仍然贴着内部运行态结构
- 后续继续重构 `engine` 时，TUI contract 仍会跟着飘

结论：不采用。

### 方案 B：轻量双轨，读写边界和协议投影边界同时收敛

做法：

- 写侧只保留 typed command / observation 提交
- 查询侧从内部事实读取，再交给 projector 输出读模型
- HTTP / WebSocket 都只面向 projector 产出的读模型
- 不引入 `CommandBus`，不引入独立 projection store

优点：

- 解决当前最关键的边界问题
- 不会为未来多个读侧和回放机制过早付成本
- 能为后续 `engine` 内部演化提供稳定缓冲层

问题：

- 需要重画整个协议契约
- 需要重写 HTTP / WebSocket 测试和 TUI 适配

结论：采用。

### 方案 C：直接升级为完整 CQRS

做法：

- 引入 `CommandBus`
- 引入 `CommandHandler`
- 查询只读取独立 projection store
- WebSocket 改为订阅读模型主题

优点：

- 最终形态最完整
- 为多读侧、回放和订阅机制预留空间最大

问题：

- 会把“上层边界重画”和“系统运行方式升级”绑成一次大改
- 当前单进程、单读侧阶段没有足够收益
- 会引入 projection lag、projection rebuild、topic 语义等额外复杂度

结论：不采用。

## 5. 采用方案

第二阶段采用方案 B：轻量双轨。

核心判断：

- 当前最重要的新抽象不是 dashboard，也不是 command bus
- 当前最重要的新抽象是 `projector`

它的职责不是写状态，而是回答一个简单问题：

> 内部已经提交的事实，TUI 现在应该看到什么？

第二阶段以后，写侧和读侧的边界变成：

- 写侧负责让事实成立
- 读侧负责解释事实

## 6. 模块所有权

### 6.1 `grid-engine`

拥有：

- `GridRuntime`
- `GridTransition`
- 领域事件
- effect 描述

不拥有：

- 外部协议 DTO
- TUI 读模型
- WebSocket 推送事件形状

### 6.2 `grid-storage`

拥有：

- 快照
- 领域事件持久化
- effect outbox 持久化

不拥有：

- 协议投影逻辑
- TUI 展示语义

### 6.3 `grid-server/write`

建议引入 `GridWriteService`。

它只负责：

- 接收 typed command 或 runtime observation
- 调用 `GridManager`
- 提交写侧事务
- 发出内部“提交完成”通知

它不负责：

- 查询
- 协议 DTO 组装
- WebSocket 对外消息格式

### 6.4 `grid-server/query`

建议引入 `GridQueryService`。

它只负责：

- 读取内部事实
- 按查询需要组织 projector 输入

它返回的不是 public DTO，而是 projector 输入模型。

为了避免查询侧重新耦合写仓储细节，第二阶段同时建议引入只读仓储端口，例如：

```rust
#[async_trait]
pub trait GridReadRepositoryPort: Send + Sync {
    async fn list_grid_snapshots(&self) -> Result<Vec<StoredGridSnapshot>>;
    async fn load_grid_snapshot(&self, grid_id: &GridId) -> Result<Option<StoredGridSnapshot>>;
    async fn list_recent_grid_events(
        &self,
        grid_id: &GridId,
        limit: usize,
    ) -> Result<Vec<StoredDomainEvent>>;
    async fn list_recent_grid_effects(
        &self,
        grid_id: &GridId,
        limit: usize,
    ) -> Result<Vec<PersistedGridEffect>>;
}
```

重点不是名字，而是 ownership：

- query 侧通过只读接口拿事实
- 不直接复用写侧事务接口
- projector 不知道存储细节

其中 `StoredGridSnapshot` 只是在 query 边界携带 `snapshot + updated_at` 的内部记录，不是 public DTO。

### 6.5 `grid-server/projector`

建议引入 `GridProjector`。

它只负责：

- 把内部读源投影成外部协议读模型

它不负责：

- 写侧提交
- 持久化
- transport

### 6.6 `server/http` 与 `server/websocket`

只负责：

- 请求解析
- 调用 query / write 服务
- 返回 projector 产物

它们不再直接依赖 `GridRuntimeSnapshot` 和 `DomainEvent` 这些内部类型。

## 7. 新的内部应用链路

### 7.1 写侧链路

统一为：

1. HTTP 命令或 runtime observation 进入 `GridWriteService`
2. `GridWriteService` 调用 `GridManager`
3. `GridWriteService` 提交写侧事务
4. 提交完成后发出内部通知

建议第一版内部通知保持很薄，例如：

```rust
pub enum GridInternalNotification {
    GridWriteCommitted { grid_id: GridId },
    GridEffectStateChanged { grid_id: GridId },
}
```

这里的关键是：

- 内部通知不是 public contract
- 它只说明“哪些 grid 的读模型可能变了”
- 它不直接决定对外 WS payload
- 第一版优先表达“需要重新投影哪些视图”，不在通知层承载活动流增量语义

### 7.2 查询链路

统一为：

1. HTTP 查询进入 `GridQueryService`
2. `GridQueryService` 通过 `GridReadRepositoryPort` 加载内部读事实
3. `GridProjector` 把内部事实投影成 public DTO
4. transport 返回 DTO

第二阶段不引入 projection store，按需投影即可。

### 7.3 WebSocket 链路

统一为：

1. 收到内部通知
2. 查询服务重新加载受影响 grid 的内部读事实
3. projector 生成外部流式事件
4. WebSocket 推送读模型更新

这意味着：

- WS 推送的是读模型变化
- 不再直接广播领域事件
- 第一版优先推完整视图片段变化，不强求单条活动增量

## 8. `GridProjector` 的输入和接口

第二阶段可以先定义一个内部读源对象：

```rust
pub struct GridReadModelSource {
    pub snapshot: GridRuntimeSnapshot,
    pub snapshot_updated_at: DateTime<Utc>,
    pub recent_domain_events: Vec<StoredDomainEvent>,
    pub recent_effects: Vec<PersistedGridEffect>,
}
```

这只是 projector 输入，不是 public contract。

projector 接口建议保持纯函数风格：

```rust
pub trait GridProjector {
    fn project_list_item(&self, source: &GridReadModelSource) -> GridListItemView;
    fn project_detail(&self, source: &GridReadModelSource) -> GridDetailView;
    fn project_activity(&self, source: &GridReadModelSource) -> Vec<GridActivityItemView>;
}
```

设计重点：

- projector 吸收对外解释复杂度
- transport 不需要知道内部事实如何映射
- TUI 不需要理解 outbox、pending order、领域事件之间的关系
- `recent_domain_events` 与 `recent_effects` 只是投影材料，不直接等于活动流输出
- 列表投影如果需要稳定 `updated_at` 或执行摘要，可以读取持久化 snapshot 时间和一个小的 recent effect 窗口

## 9. 新的对外协议

第二阶段不考虑兼容性，直接重画 contract。

### 9.1 HTTP 查询

保留两个查询入口：

- `GET /grids`
- `GET /grids/:id`

其中：

- `GET /grids` 返回 `GridListResponse`
- `GET /grids/:id` 返回 `GridDetailView`

原有 `GET /grids/:id/snapshot` 删除。

### 9.2 HTTP 写入

保留：

- `POST /grids/:id/commands`

但命令请求体从字符串改成 typed command：

```rust
pub struct GridCommandRequest {
    pub command: GridCommandType,
}

pub enum GridCommandType {
    Pause,
    Resume,
    Reconcile,
}
```

响应保持“已接受”语义，不直接返回最新详情：

```rust
pub struct GridCommandAccepted {
    pub grid_id: String,
    pub command: GridCommandType,
    pub accepted: bool,
}
```

这样写侧接口只回答：

- 命令是否被接受

最新读模型仍通过 `GET` 或 `WS` 获取。

### 9.3 WebSocket

WebSocket 不再暴露 `DomainEvent`，改成统一流式 envelope：

```rust
pub struct GridStreamEvent {
    pub grid_id: String,
    pub payload: GridStreamPayload,
}

pub enum GridStreamPayload {
    GridListItemChanged { item: GridListItemView },
    GridDetailChanged { detail: GridDetailView },
}
```

第一版只需要这两类推送。

第二阶段第一版不单独定义 `GridActivityAppended`，原因是：

- 当前没有独立 projection store
- 当前也不打算引入订阅 cursor
- 如果直接推“活动追加”，实现阶段很容易出现重复追加或顺序不清

因此先采用更稳的策略：

- activity 作为 `GridDetailView` 的一部分返回
- WebSocket 通过 `GridDetailChanged` 让 TUI 刷新详情中的活动块

如果后续真的需要更细的活动流增量，再在具备稳定 cursor 后单独升级。

## 10. 读模型定义

### 10.1 `GridListItemView`

列表项只保留稳定主视图信息：

```rust
pub struct GridListItemView {
    pub id: String,
    pub instrument: InstrumentView,
    pub lifecycle: GridLifecycleView,
    pub reference_price: Option<f64>,
    pub exposure: ExposureSummaryView,
    pub execution: ExecutionBadgeView,
}
```

设计重点：

- 列表不再原样暴露 `pending_order`
- `execution` 只表达当前执行摘要

### 10.2 `GridDetailView`

详情页按块组织：

```rust
pub struct GridDetailView {
    pub identity: GridIdentityView,
    pub status: GridStatusPanelView,
    pub strategy: GridStrategyView,
    pub market: GridMarketView,
    pub position: GridPositionView,
    pub execution: GridExecutionView,
    pub activity: Vec<GridActivityItemView>,
    pub available_commands: Vec<GridCommandView>,
}
```

设计重点：

- 不再暴露一个扁平 `snapshot`
- `available_commands` 由 projector 直接给出，TUI 不复制业务判断

### 10.3 `GridExecutionView`

对外统一执行视图：

```rust
pub struct GridExecutionView {
    pub state: ExecutionStateView,
    pub pending_order: Option<OrderExecutionView>,
}
```

这里的关键是：

- 内部的 `pending_order + persisted_effect`
- 对外先收敛成一个稳定执行块
- TUI 只展示高层执行语义，不直接暴露内部追踪字段

### 10.4 `GridActivityItemView`

对外统一活动流：

```rust
pub struct GridActivityItemView {
    pub ts: String,
    pub message: String,
    pub level: ActivityLevelView,
}
```

它可以吸收：

- 领域事件
- effect 执行成功 / 失败
- 后续如果需要，再继续吸收关键订单或仓位观察更新

活动流不再直接等于 `DomainEvent`。

## 11. 错误处理

第二阶段按边界分层处理错误：

- `CommandError`
  - 命令无效
  - grid 不存在
  - 命令当前不可执行
- `QueryError`
  - 查询目标不存在
- `ProjectionError`
  - 内部事实无法被合法投影
  - 默认视为实现错误，而不是业务常态

设计原则：

- “当前没有参考价”不是错误，而是合法空值
- “当前没有待处理委托”不是错误，而是 `execution.pending_order = None`
- “命令不可用”优先由 `available_commands` 表达，而不是让 TUI 自己推断

## 12. 测试策略

按项目约束，第二阶段先补验收测试，再重构实现。

### 12.1 projector 验收测试

新增并先写失败测试，覆盖：

- 活动中 grid 的列表项投影
- 含 effect 执行状态的详情投影
- 活动流如何吸收领域事件和 effect 失败

重点锁定：

- 列表看什么
- 详情看什么
- 活动流看什么

### 12.2 HTTP contract 验收测试

覆盖：

- `GET /grids` 返回 `GridListResponse`
- `GET /grids/:id` 返回 `GridDetailView`
- `POST /grids/:id/commands` 接受 typed command
- grid 不存在、命令无效、命令不可执行的错误形状

### 12.3 WebSocket contract 验收测试

覆盖：

- 写侧提交后推送 `GridListItemChanged`
- 详情变化时推送 `GridDetailChanged`
- effect 失败或关键 observation 变化时，`GridDetailChanged` 会带出新的活动块

重点验证：

- WS 推送的是读模型变化
- 不是内部领域事件广播

### 12.4 服务边界测试

覆盖：

- `GridWriteService` 不再暴露查询接口
- `GridQueryService` 不依赖写侧 mutation lock
- `http/ws` 不再直接依赖内部 snapshot mapper

## 13. 实施顺序

建议按这个顺序执行：

1. 先定义新协议类型
   - 新建读模型 DTO
   - 新建 typed command DTO
   - 先写 projector 验收测试
2. 拆 `GridPlatformService`
   - 引入 `GridWriteService`
   - 引入 `GridQueryService`
3. 引入 `GridProjector`
   - 让 query 返回内部读源
   - 让 projector 负责 public DTO
4. 重写 HTTP
   - 删除 `/grids/:id/snapshot`
   - 改成 `/grids/:id`
   - `POST /commands` 改 typed command
5. 重写 WebSocket
   - 从领域事件广播改成读模型更新推送
6. 清理旧协议
   - 删除 `GridSnapshot` 风格 contract
   - 删除旧 mapper 和兼容测试
7. 同步 TUI
   - 只依赖新读模型 contract

这个顺序的重点是：

- 先锁住目标协议
- 再拆服务边界
- 最后删除旧 contract

## 14. 验收标准

第二阶段完成后，应满足：

- `GridPlatformService` 被拆解，不再同时承担写侧、查询和协议映射
- 外部协议不再暴露 `GridSnapshot`、原始 `PendingOrder`、原始 `DomainEvent`
- HTTP 查询改为 `GET /grids` 和 `GET /grids/:id`
- HTTP 命令改为 typed command
- WebSocket 推送改为读模型更新，而不是领域事件广播
- 查询侧通过独立只读仓储接口组织 projector 输入，不重新耦合写侧事务接口
- TUI 只依赖稳定读模型，不直接理解内部运行态结构
- 全量测试通过，并新增 projector / HTTP / WS 验收测试

## 15. 为什么这一步先于 `engine` 内聚升级

第二阶段故意不先把更多运行时知识推回 `engine`，原因是：

- 第一阶段已经把写侧提交边界固定下来
- 现在最急的是隔离内部结构与外部协议
- 如果在没有 projector 缓冲层的前提下继续改 `engine`，协议层还会持续跟着抖动

先把应用投影边界做对，后续再处理更深的 `engine` 内聚，改动面会更可控。
