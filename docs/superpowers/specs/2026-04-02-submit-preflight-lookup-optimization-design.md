# Submit Preflight Lookup 优化设计

> 更新：2026-04-14 起，`SubmitPreflight` 的运行时维护边界从“由 `recovery` 任务收到通知后顺手重算”调整为“独立的脏标记/消费者”；同日又把 submit 生命周期接口下沉到 `application/src/submit_effect_service.rs`，不再由 `TrackEffectService` 暴露 submit-specific 协议。随后又在 `server` 侧引入 `SubmitCoordinator` / `SubmitFlight` / `SubmitCompletion`，由它们组合 `SubmitPreflight` 和 `SubmitDispatch`，把 `mark_submit_started(...)` 这类运行时 started 语义以及“一次 started submit 只能结束一次”的约束都留在 server 层。再往下一层，`application::SubmitDispatch` 自身也已经收紧成 one-shot handle，避免将来出现绕过 coordinator 的重复终态写回。最新的 recovery 协调边界见 [Recovery 协调与读侧广播分离设计](2026-04-14-recovery-submit-preflight-decoupling-design.md)。

## 背景

当前 `server/src/effect_worker.rs` 在执行每一条 `SubmitOrder` effect 前，都会先调用一次 `exchange.get_open_orders(...)`，再从返回结果里按 `client_order_id` 找匹配的 live order，最后把这个可选 live order 传给 `write_service.prepare_submit_execution(...)`。

这样做的目的不是策略计算，而是 submit 幂等恢复：

- 如果这条 persisted submit effect 对应的真实订单已经在交易所活着，本地可以先走恢复，不重复下单。
- 如果服务重启后重新捞到旧的 pending submit effect，本地可以先看交易所状态，再决定是恢复还是继续执行。

这条保护是有效的，但代价也很直接：

- 每次正常 submit 都会多一次签名 `GET /openOrders`
- submit 延迟增加
- 对 `recvWindow`、时间同步和签名 GET 可用性的依赖被放大

这次优化的目标，是减少“每次正常 submit 都查一次 `openOrders`”的成本，同时保留 submit 幂等恢复。

## 目标

- 让新鲜的正常 submit 默认不再执行 `openOrders` 预检查。
- 保留对“可疑 submit”的交易所 live order 保护，避免重复下单。
- 不把交易所查询逻辑散到 executor 或更多调用点。

## 非目标

- 不修改 executor 的 `recover_submit_effect(...)` 语义。
- 不引入新的完整订单台账。
- 不依赖低频 `openOrders` 缓存替代 submit 前查询。

## 代码现状与约束

### 1. `attempt_count` 不能作为主要信号

虽然直觉上“重试过的 submit 才需要查交易所”很合理，但当前 `attempt_count` 不能准确表达这个语义。

原因是：

- `list_dispatchable_effects()` 只返回 `Pending` effect。
- effect 一旦走 `complete_effect_failed(...)`，状态会变成 `Failed`，不再进入 dispatchable 集合。
- 真正危险的场景，是“这条 submit 已经尝试过，但因为 writeback/cleanup/persistence 问题仍然保持 `Pending`”。这类 effect 仍可能 `attempt_count == 0`。

因此，`attempt_count > 0` 不能覆盖“同一进程内已经尝试过 submit、但 effect 仍是 pending”的重复下单风险。

### 2. 需要区分两类 submit

- **新鲜 submit**
  - 本进程里第一次尝试执行
  - 没有必要先查一次 `openOrders`
- **可疑 submit**
  - 服务重启后恢复的旧 pending effect
  - 或本进程里已经尝试过一次、但 effect 仍然保持 pending
  - 需要先查 `openOrders`，避免重复下单

## 备选方案

### 方案 A：继续在 `effect_worker` 里直接写 if 规则

- 在 `effect_worker` 里按 `created_at`、`attempt_count`、其他标记直接决定是否查 `openOrders`

优点：

- 改动最小

缺点：

- “为什么这次 submit 需要交易所查询”的知识会停留在 worker
- 以后规则一多，容易继续长成条件分支堆积

### 方案 B：显式的 submit preflight 决策模块

- 新增一个小而明确的决策模块，输出 `SubmitPreflightDecision`
- `effect_worker` 只负责执行这个决策

优点：

- “何时需要交易所 live order 保护”有单一归属
- 可以把运行时知识和 effect 元数据一起封装
- `effect_worker` 只做执行，不背恢复判断

缺点：

- 比方案 A 多一个小模块

### 方案 C：完整 submit 恢复台账

- 给 submit effect 引入更明确的持久化执行生命周期，再据此决定是否查交易所

优点：

- 语义最强

缺点：

- 明显超出本次优化范围
- 改动面太大

## 结论

采用 **方案 B：显式的 submit preflight 决策模块**。

## 设计

### 新模块

新增 `server/src/submit_preflight.rs`，负责输出：

- `SubmitPreflightDecision::Direct`
- `SubmitPreflightDecision::NeedsLiveOrderLookup { client_order_id: String }`

这个模块的职责是：

- 根据 persisted submit effect 元数据
- 再结合 runtime 启动时显式采样到的 pending submit 集合
- 以及当前进程内“这条 effect 是否已经尝试过 submit”
- 决定这次 submit 是否需要交易所 `openOrders` 保护

### 模块边界

- `effect_worker`
  - 只负责调用 preflight 决策
  - 根据决策选择是否调用 `exchange.get_open_orders(...)`
  - 不自己写恢复启发式

- `submit_preflight`
  - 拥有“什么时候 submit 需要交易所 live order 查询”这份知识
  - 吸收启动恢复集合和运行时已尝试 effect 的本地跟踪
  - 在当前单 worker 顺序执行模型下，提供清晰但简单的协调接口

- `SubmitEffectService`
  - 继续接受 `Option<&ExchangeOrder>` 作为可选 live order 证据
  - 输出 `SubmitAttempt::{Dispatch, Finished}`，把是否继续发单以及后续写回所需上下文一起封装
  - 拥有 submit 生命周期变化与 pending submit 集合语义变化之间的映射

- `executor`
  - 继续只处理“本地状态 + 可选 live order”
  - 不新增交易所访问职责

### 状态存放位置

`submit_preflight` 持有的共享状态需要挂在 `ServerState`，由 `ServerRuntime::start()` 负责初始化，并由 `EffectWorker` 共享使用。这样：

- runtime 可以在启动阶段写入 `startup_pending_submit_effects`
- effect worker 只在真实 submit 即将发生前记录 `attempted_submit_effects`
- runtime 通过独立 worker 统一做 preflight 缓存的删除和重算
- 两处不会各自持有一份集合副本

## 决策规则

第一版只保留两种决策结果，风险来源有两类：

### `NeedsLiveOrderLookup`

满足任一条件：

1. 这条 `effect_id` 属于 `startup_pending_submit_effects`
   - 表示这是 runtime 启动前已经存在、并在启动时显式采样到的 pending submit effect
   - 属于重启恢复场景

2. 这条 `effect_id` 已经在当前进程里执行过一次 `submit_order(...)`
   - 但 effect 仍然保持 `Pending`
   - 属于“同进程内可重复 submit”的危险场景

### `Direct`

其余情况全部直接执行。

也就是：

- 新生成
- 当前进程第一次尝试执行
- 没有恢复风险证据

就不查 `openOrders`，直接走：

- `prepare_submit_execution(..., None)`
- `submit_order(...)`

## 运行时已尝试 submit 的跟踪

`submit_preflight` 需要维护两份进程内集合：

- `startup_pending_submit_effects: HashSet<String>`
- `attempted_submit_effects: HashSet<String>`

它们分别表达两件事：

- `startup_pending_submit_effects`
  - “这条 pending submit 是 runtime 启动前就已经存在的恢复对象”
- `attempted_submit_effects`
  - “这个 effect 在当前进程内，是否已经真的开始过一次 submit 执行”

### `startup_pending_submit_effects` 的来源

这份集合不依赖时间比较推断，而是由 runtime 在启动时显式采样：

1. `ServerRuntime::start()` 在完成 `startup_sync` 和 `replay_startup_user_data` 之后、启动任何长生命周期后台任务之前，调用一次新的仓储接口，例如 `list_all_pending_submit_effects()`，列出**所有 pending submit effects**
2. 这份集合不要求“当前可调度”，只要求：
   - `status == Pending`
   - `effect == TrackEffect::SubmitOrder`
3. 把对应 `effect_id` 放入 `startup_pending_submit_effects`

这样“启动恢复 submit”变成 runtime 明确提供的一份事实，而不是 `created_at < runtime_started_at` 这种 wall-clock 代理，也不会遗漏“启动时尚不可调度、后续才变成 dispatchable”的旧 submit。这里的要求是：采样必须发生在 `recovery task`、`effect worker`、`user task`、`market task` 这些可能继续推进 submit 生命周期的后台任务启动之前。

### preflight 缓存的删除归属

为了避免把同一份缓存的失效规则拆到 submit worker 和 runtime 两处，当前归属拆成两层：

- `SubmitEffectService` 返回“这次 submit 尝试是否继续 dispatch，以及最终是否影响 pending submit 集合”这个事实
- runtime 的独立 `submit_preflight` worker 读取 dirty 标记后，重新读取当前 pending submit 集合
- `submit_preflight` 提供统一的重算接口，例如：
  - `reconcile_pending_submit_effects(current_pending_submit_effect_ids)`

这个接口负责：

- 清掉已经不再属于 pending submit 集合的 `startup_pending_submit_effects`
- 清掉已经不再属于 pending submit 集合的 `attempted_submit_effects`

### 单 worker 不变量

当前设计依赖一个明确不变量：

- 同一 `ServerRuntime` 内只有一个 `EffectWorker`
- `EffectWorker::run_once()` 顺序处理 dispatchable effects
- 同一条 pending submit effect 不会被两个并发执行者同时推进

在这个不变量下，`submit_preflight` 内部仍可以保持两步式状态接口：

- `decide(...)`
- `mark_submit_started(effect_id)`

但当前实现不再让 `effect_worker` 直接消费这两步；它们由 `server` 层的 `SubmitCoordinator::prepare(...)` 组合成一次性的 server 语义入口。

如果未来引入并发 effect worker 或并发 `process_effect`，这部分接口需要升级成原子协调接口；那会是单独的设计变更，不在本次范围内。

### `attempted_submit_effects` 的写入时机

`SubmitCoordinator::prepare(...)` 内部维持的精确顺序必须是：

1. `recover_or_dispatch(...)`
2. 若返回 `SubmitAttempt::Dispatch(...)`
3. 调 `mark_submit_started(effect_id)`
4. 返回 `SubmitFlight`
5. `SubmitFlight` 再拆成 `OrderRequest + SubmitCompletion`
6. 再调用真实 `submit_order(...)`

不能在 `recover_or_dispatch(...)` 之前标记 started，因为这一步可能直接结束当前 effect；此时并没有发生真实 submit，也不该把 effect 记成 in-flight。

### `attempted_submit_effects` 的清理时机

当这条 effect 已经不再保持 pending 时，清掉这条记录：

- submit succeeded
- submit superseded
- submit failed 并已持久化失败

### 保留时机

如果这次 submit 已经开始，但 effect 仍保持 `Pending`，则保留这条记录：

- receipt/writeback 持久化失败
- cleanup 持久化失败
- 其他“本次真实 submit 已经发生，但 effect 还没完成”的场景

这样下一次 worker 再碰到这条 effect 时，就会自动走 `NeedsLiveOrderLookup`。

### 清理路径对齐

实现时需要把集合清理和具体 effect 状态变化路径一一对齐，至少覆盖：

- `complete_submit_execution(...)` 成功
- submit recovery 返回 `Superseded`
- `record_submit_failure(...)` 已持久化失败
- submit 已不再属于 pending/dispatchable 集合的其他明确写回路径

这里的要求不是引入更多状态，而是把“集合何时清理”绑定到 runtime 的单一重算入口，避免一部分在 write side 清、一部分在 runtime 清。

## 数据流

### 新鲜 submit

1. `effect_worker` 取到一条 pending submit effect
2. 调 `SubmitCoordinator::prepare(...)`
3. 内部得到 `Direct`
4. 内部执行 `recover_or_dispatch(..., None)`
5. 若需要真实 submit，则内部先 `mark_submit_started(effect_id)`，再返回 `SubmitFlight`
6. `effect_worker` 从 `SubmitFlight` 拿到 `OrderRequest + SubmitCompletion`
7. `effect_worker` 执行 `submit_order(...)`

### 启动后恢复旧 submit

1. `effect_worker` 取到一条 pending submit effect
2. 调 `SubmitCoordinator::prepare(...)`
3. 内部得到 `NeedsLiveOrderLookup`
4. 内部查询一次 `openOrders`
5. 内部只找 `client_order_id` 对应的 live order
6. 内部执行 `recover_or_dispatch(..., live_order)`
7. 若已恢复则不再重复 submit；否则返回 `SubmitFlight` 继续执行

这里的“启动后恢复旧 submit”不要求这条 effect 在启动时已经 dispatchable，只要求它在 runtime 启动阶段已被识别为“启动前遗留的 pending submit”。

### 同进程内再次碰到旧 pending submit

1. 某次真实 submit 已经开始
2. 但 effect 因 writeback/persistence 问题仍保持 `Pending`
3. 下一轮 worker 再拿到这条 effect
4. `submit_preflight` 因 `attempted_submit_effects` 命中，返回 `NeedsLiveOrderLookup`
5. 先查交易所再决定是否继续 submit

## 为什么不采用低频缓存

这次不选择“submit 改读低频 `openOrders` 缓存”，原因是：

- 缓存会引入另一套“快照新鲜度”语义
- 调用方需要开始理解缓存时效和缺失情况
- 比起“只对可疑 submit 查一次 live order”，缓存并没有更简单

当前目标只是让正常 submit 不再每次打一次签名 GET，不是把交易所 live order 查询改造成共享缓存层。

## 测试

至少补这四类验收测试：

1. **新鲜 submit 不查 `openOrders`**
   - 新生成 pending submit
   - 当前进程第一次执行
   - 断言 `get_open_orders_calls == 0`

2. **启动时显式采样到的 pending submit 会查 `openOrders`**
   - `effect_id` 已被放入 `startup_pending_submit_effects`
   - 断言 `get_open_orders_calls == 1`

3. **同进程重复执行的 pending submit 会查 `openOrders`**
   - 第一次真实 submit 已开始，但 effect 仍保持 pending
   - 第二次 `process_effect / run_once` 相对第一次执行前，`get_open_orders_calls` 增量为 `1`

4. **遗留 submit 在交易所已有 matching live order 时不会重复下单**
   - `client_order_id` 匹配 live order
   - 最终应走恢复，而不是再次 `submit_order(...)`
   - 这个用例优先复用现有“重启后恢复 pending effect”测试骨架，而不是新造一套 runtime fixture

## 风险与后续

这次设计仍保留一个有意为之的限制：

- 是否需要交易所 live order 查询，仍然基于启发式判断
- 不是完整的 submit 生命周期台账

但相比当前“每次 submit 都查”，它有两个明显好处：

- 把大多数新鲜 submit 的签名 GET 去掉
- 同时保住来自两类来源的高风险 submit：
  - 重启恢复
  - 同进程已尝试但仍 pending

如果后面还要继续演进，下一步应该是：

- 把 `attempted_submit_effects` 进一步升级成更明确的 submit 执行状态
- 而不是继续增加更多 if 规则
