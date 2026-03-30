# 写侧边界收敛设计

基于现有架构评审结论，先处理上层写侧边界问题，再进入 `engine` 内部收敛。本文档只覆盖第一阶段：让状态变更、持久化和 effect 执行形成清晰的应用边界。

相关背景：

- 当前整体架构见 [`2026-03-24-grid-platform-architecture-design.md`](2026-03-24-grid-platform-architecture-design.md)
- 当前运行时边界设计见 [`2026-03-25-grid-runtime-boundary-redesign.md`](2026-03-25-grid-runtime-boundary-redesign.md)

## 1. 背景

当前主流程已经统一成 `GridId`、`Instrument`、`GridRuntime`、`GridManager` 这一套名义边界，但写侧仍然存在一组没有收敛的知识：

- `server` 先通过 `engine` 计算 `GridTransition`
- `server` 先持久化快照和领域事件
- `server` 再在事务外执行撤单/下单
- `server` 再用额外 mutation 补写 `pending_order`
- WebSocket 广播既依赖领域事件，也依赖 `SnapshotUpdated`

这会持续放大三个复杂度症状：

- `change amplification`：改事务、改 effect 执行、改广播时都要同时改 `server` 多处
- `cognitive load`：开发者要同时记住“什么时候算 transition、什么时候持久化、什么时候补写 pending order”
- `unknown unknowns`：进程在快照已落库、订单未执行，或订单已执行、状态未补写时崩溃，会留下不清晰的恢复状态

当前代码能通过测试，但“能跑”不等于写侧边界已经正确。

## 2. 设计目标

### 2.1 主目标

- 写侧只有一个权威入口负责网格状态变更
- `GridTransition` 里的 `snapshot`、`events`、`effects` 要么一起提交，要么一起不提交
- effect 执行不再依赖“内存里刚算出的 transition 仍然有效”这个隐式前提
- `server` 运行时崩溃后，系统可以从持久化状态恢复 effect 执行
- 为下一阶段拆 `GridPlatformService` 和后续把更多运行时知识推回 `engine` 提供稳定边界

### 2.2 非目标

- 这次不直接拆 `GridPlatformService` 的全部读侧职责
- 这次不引入第二个交易所
- 这次不把所有订单生命周期逻辑都收回 `engine`
- 这次不改对外 HTTP / WebSocket 协议

## 3. 备选方案

### 方案 A：保留当前事务外 effect 执行，只拆服务文件

做法：

- 先把 `GridPlatformService` 按查询、写侧、protocol 映射拆成多个文件
- 继续维持“先保存快照，再执行 effect，再补写 `pending_order`”的流程

优点：

- 改动最小
- 测试迁移成本低

问题：

- 只解决文件过厚，不解决写侧恢复边界
- effect 执行仍然依赖进程内时序
- 后续再补 outbox 时还要重新改写服务边界

结论：不采用。

### 方案 B：引入写侧 outbox，先收紧应用边界

做法：

- 保持 `engine` 继续产出 `GridTransition`
- `server` 写侧把 `snapshot`、`events`、`effects` 一起持久化
- 运行时执行器只消费已提交的待执行 effect
- effect 执行结果再通过 observation / command 回流写侧

优点：

- 先解决第一轮 review 的主问题
- effect 执行从“内存动作”变成“可恢复任务”
- 下一轮拆 query / projector 时不需要再改写事务边界

问题：

- 需要扩展 `storage` schema 和 repository 接口
- 需要重写 `server/runtime` 的执行链

结论：采用。

### 方案 C：直接把更多运行时状态机逻辑推回 `engine`

做法：

- 同时重构 `GridRuntime`、`GridManager`、`server/runtime`、`storage`
- 让 `engine` 立刻成为完整运行时聚合

优点：

- 最终结构更干净

问题：

- 同时改上层边界和内部聚合，改动面过大
- 很难区分“写侧边界问题”和“状态机聚合问题”
- 探索阶段不适合先走这条路

结论：作为下一阶段方向，不作为当前第一步。

## 4. 采用方案

第一阶段采用方案 B：先引入写侧持久化 outbox，收紧写侧应用边界。

核心判断：

- 当前最重要的 abstraction 不是新的协议 mapper，也不是更深的 `GridRuntime`
- 当前最重要的 abstraction 是“写侧提交”本身

这个 abstraction 应该回答一个简单问题：

> 一个网格状态变更一旦对系统可见，到底有哪些东西已经成为持久化事实？

答案应当是：

- 新快照
- 领域事件
- 待执行 effect

这三者一起提交，才能形成稳定的写侧边界。

## 5. 新边界定义

### 5.1 新的一等应用抽象

引入写侧应用服务，建议命名为 `GridWriteService`。

它只负责：

- 接收 `GridObservation` 或 `GridCommand`
- 调用 `GridManager` 生成 `GridTransition`
- 原子保存 `snapshot + events + effects`
- 在提交后发出“已提交事件”通知

它不负责：

- 直接执行交易所 effect
- 对外 protocol DTO 映射
- 列表查询和快照查询

### 5.2 提交后的 effect 不再是瞬时值

`GridTransition.effects` 在第一阶段必须被提升为持久化对象。

建议新增持久化模型：

```rust
pub struct PersistedGridEffect {
    pub effect_id: String,
    pub track_id: GridId,
    pub effect: GridEffect,
    pub status: EffectStatus,
    pub created_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub attempt_count: u32,
}

pub enum EffectStatus {
    Pending,
    Executing,
    Succeeded,
    Failed,
}
```

这里的关键不是字段名字，而是 ownership：

- effect 的生命周期从“函数返回值”变成“仓储中的事实”
- 第一阶段实际只使用 `Pending / Succeeded / Failed`
- `Executing` 只保留给未来引入 lease / timeout 恢复策略时扩展；当前实现不会在交易所调用前把 effect 提前改成 `Executing`
- `server/runtime` 不再拥有 effect 的存在性，只拥有 effect 的执行职责

### 5.3 写侧提交接口

建议把当前 `StateRepositoryPort` 升级为包含 outbox 的写侧仓储接口：

```rust
pub struct GridWriteBatch {
    pub snapshot: GridRuntimeSnapshot,
    pub events: Vec<DomainEvent>,
    pub effects: Vec<GridEffect>,
}

#[async_trait]
pub trait GridWriteRepositoryPort: Send + Sync {
    async fn commit_grid_write(
        &self,
        track_id: &GridId,
        batch: &GridWriteBatch,
    ) -> Result<CommittedGridWrite>;

    async fn load_grid_state(&self, track_id: &GridId) -> Result<Option<GridRuntimeSnapshot>>;

    async fn list_pending_effects(&self) -> Result<Vec<PersistedGridEffect>>;
    async fn mark_effect_executing(&self, effect_id: &str) -> Result<()>;
    async fn mark_effect_succeeded(&self, effect_id: &str) -> Result<()>;
    async fn mark_effect_failed(&self, effect_id: &str, error: &str) -> Result<()>;
}
```

`CommittedGridWrite` 可以在第一阶段保持很薄，只要能返回新快照、领域事件和已落库 effect 标识即可。

## 6. 运行时执行链重画

### 6.1 当前链路

当前链路是：

1. 市场事件进入 `server/runtime`
2. 调用 `GridPlatformService`
3. 生成 `GridTransition`
4. 保存 `snapshot + events`
5. 立即执行 `effects`
6. 额外补写 `pending_order`

问题在于第 5、6 步不属于同一个提交边界。

### 6.2 第一阶段新链路

新的链路改成：

1. 市场事件进入写侧服务
2. `engine` 生成 `GridTransition`
3. 原子保存 `snapshot + events + effects`
4. 写侧服务只广播“已提交事件”
5. effect 执行器从仓储读取 `Pending` effect
6. `SubmitOrder` 先把 `pending_order=Submitting` 写回快照，作为崩溃恢复锚点
7. 执行交易所调用
8. 成功时写回订单回执并标记 `Succeeded`；失败时标记 `Failed`
9. 在没有 lease / timeout 恢复策略前，effect 保持 `Pending`，不提前标记 `Executing`
10. 交易所结果通过 observation / command 回流写侧

设计约束：

- 执行器只能执行已提交 effect
- 任何 effect 执行失败都不能回滚已提交快照
- 状态修正只能通过新的写侧输入推进，不能绕过写侧服务直接改 manager
- `Submitting` pending order 是第一阶段的恢复锚点；只要对应 submit effect 还未终态，就不能在启动对齐时提前清掉

## 7. `pending_order` 的第一阶段处理

第一阶段不要求把完整订单生命周期都推回 `engine`，但要取消当前“交易所动作成功后立刻补写 pending order”的额外隐式流程。

调整原则：

- `SubmitOrder` effect 提交后，系统已经知道“存在待执行下单意图”
- 如果需要界面展示“正在提交”，这应该来自 effect 状态或一条显式运行时状态，而不是运行时外部补丁
- 真正的交易所订单号、订单状态，仍由订单 observation 回流写侧后再更新网格状态

这意味着：

- 第一阶段允许 `pending_order` 的语义先收缩
- 不再要求“提交请求后马上伪造一个最终 pending order”
- 订单号写入 state 的时机改为：收到交易所回执或用户流 observation 后

这样可以先去掉最危险的事务外补写。

## 8. 模块所有权

### 8.1 `poise-engine`

拥有：

- `GridTransition`
- `GridRuntimeSnapshot`
- 领域事件
- effect 的纯描述

不拥有：

- effect 持久化状态
- effect 执行
- 对外协议 DTO

### 8.2 `poise-storage`

拥有：

- 原子提交 `snapshot + events + effects`
- effect outbox 状态推进

不拥有：

- effect 执行策略
- 协议投影

### 8.3 `poise-server`

写侧服务拥有：

- manager 调用
- 写侧事务边界
- 提交后通知

运行时执行器拥有：

- 拉取待执行 effect
- 执行交易所调用
- 记录执行结果

它不拥有：

- 网格状态推进规则
- 对外协议映射

## 9. 数据模型变更

SQLite 新增 `grid_effects` 表。

建议字段：

- `effect_id`
- `track_id`
- `effect_json`
- `status`
- `attempt_count`
- `last_error`
- `created_at`
- `updated_at`

约束：

- `commit_grid_write()` 必须在单事务里同时写入 `grid_snapshots`、`domain_events`、`grid_effects`
- 同一个 `effect_id` 只能落库一次
- effect 查询默认按 `status = pending` 和创建顺序返回

第一阶段不做复杂调度，不做分布式锁，不做多执行器竞争控制。默认单进程单执行器。

## 10. 对现有模块的直接影响

### 10.1 `server/src/application.rs`

第一阶段只保留写侧协调职责：

- 允许继续暂时留在同一个文件
- 但查询和 protocol 映射在实现计划里要标记为“下一阶段迁出”

原因：

- 当前真正阻塞的是写侧一致性边界
- 查询拆分不应先于写侧边界重画

### 10.2 `server/src/runtime.rs`

从“transition 直接执行器”改成“outbox effect 执行器 + observation 回流器”。

要删除的隐式知识：

- 下单成功后立刻补写 pending order
- 撤单成功后立刻直接清理运行态

### 10.3 `storage/src/sqlite.rs`

从“快照仓储”升级成“写侧事务仓储 + effect outbox 仓储”。

## 11. 验收标准

第一阶段完成后，应满足：

- 写侧单次提交会原子保存快照、领域事件和待执行 effect
- 任何交易所 effect 都来自持久化 outbox，而不是内存中尚未提交的 transition
- 进程在快照提交后、effect 执行前崩溃，重启后仍能恢复待执行 effect
- `server/runtime` 不再通过额外 mutation 直接补写 `pending_order`
- 当前 HTTP / WebSocket 协议保持不变
- 全量测试通过，并新增覆盖：
  - outbox 原子提交
  - 重启后 effect 恢复
  - effect 执行失败不破坏已提交快照

## 12. 下一阶段

第一阶段完成后，再进入下一阶段：

- 从 `GridPlatformService` 拆出 `GridQueryService`
- 拆出独立 protocol projector
- 把更多订单生命周期和预算知识推回 `engine`
- 为多交易所 adapter registry 做运行时装配收敛

这次不抢跑这些内容，避免把“上层边界修正”和“内部聚合重构”混成一次大改。
