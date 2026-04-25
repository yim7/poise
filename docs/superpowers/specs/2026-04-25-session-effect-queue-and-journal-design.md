# Session Effect Queue 与持久 Journal 边界设计

**日期：** 2026-04-25
**基线：** 当前 `poise` 分支

相关文档：

- Track fresh-session 启动设计：[2026-04-23-track-session-runtime-fresh-start-design.md](2026-04-23-track-session-runtime-fresh-start-design.md)
- 执行器边界设计：[2026-04-22-curve-boundary-ledger-execution-design.md](2026-04-22-curve-boundary-ledger-execution-design.md)

相关代码：

- effect worker：[`../../../server/src/effect_worker`](../../../server/src/effect_worker)
- mutation executor：[`../../../application/src/mutation_executor.rs`](../../../application/src/mutation_executor.rs)
- effect store：[`../../../application/src/track_effect_store.rs`](../../../application/src/track_effect_store.rs)
- storage：[`../../../storage/src/sqlite.rs`](../../../storage/src/sqlite.rs)
- startup bootstrap：[`../../../server/src/runtime/startup_bootstrap.rs`](../../../server/src/runtime/startup_bootstrap.rs)

## 1. 当前共识

系统重启后必须重新计算运行环境：

1. 读取 track 配置。
2. 读取必要业务真值：控制状态、账务统计。
3. 查询交易所当前 position、open orders、rules。
4. 构造 fresh-session runtime。
5. 当前进程内重新规划下单、撤单和追单。

因此，上一进程留下的本地执行工作不是恢复输入。

这意味着：

- 旧 `Pending` effect 不应跨重启继续派发。
- 旧 `Executing` effect 不应跨重启继续等待完成。
- 旧 follow-up retirement 不应跨重启继续执行。
- 启动正确性不依赖 `track_effects`。
- 运行正确性不依赖任何旧 session 的本地状态。

## 2. 问题

当前实现还保留了旧持久 outbox 模型：

- effect worker 通过 `TrackEffectStore::list_dispatchable_effects()` 从数据库扫描 `Pending` effect。
- `track_effects` 同时承担运行队列、状态机顺序控制、UI 历史和调试记录。
- startup 还需要扫描旧 `Pending / Executing` effect 并标记为 `Superseded`。
- follow-up retirement 被持久化成跨重启任务。
- status-only 写回需要考虑多个 status 是否同事务提交。

这些复杂度来自一个已经不成立的假设：

> effect 是跨进程恢复协议的一部分。

在 fresh-session 共识下，这个假设应删除。

## 3. 目标

这次设计只解决一个边界问题：

> effect 可以被记录，但不能再作为运行恢复协议。

具体目标：

- 当前 session 的 effect 派发只依赖内存队列。
- 持久 effect 数据降级为 journal，用于 UI、诊断和审计。
- 重启后旧 effect 不派发、不恢复、不参与 startup。
- startup 不再清理旧 pending effect，也不再删除旧 follow-up retirement 来保证运行正确性。
- 账务、交易历史、PnL、fee、funding 继续持久化，并保持业务真值地位。

## 4. 非目标

- 不删除账务持久化。
- 不删除交易历史和 ledger event。
- 不改变 fresh-session 的 position/open-orders 查询模型。
- 不让内存队列跨进程恢复。
- 不在本轮重写执行器的 boundary / binding / catch-up 仲裁模型。
- 不要求持久 journal 与运行队列强一致。journal 写失败可以影响诊断完整性，但不能让 runtime 进入错误恢复路径。

## 5. 持久化分层

### 5.1 业务真值

业务真值跨重启保留，并参与 fresh-session 构造。

第一阶段保留：

- `TrackControlState`
- `TrackLedgerState`
- ledger events / trade history
- account monitor baseline 与当日统计

业务真值回答：

> 当前产品语义和账务历史是什么？

它不回答：

> 上一进程还有什么本地 effect 没执行？

### 5.2 Session runtime

Session runtime 只存在于当前进程。

包括：

- bindings
- boundary progress
- recovery anomaly
- pending effect queue
- executing effect
- follow-up retirement
- startup cleanup filter
- submit preflight in-flight 状态

这些数据重启后全部作废。

### 5.3 Effect journal

Effect journal 是历史记录，不是运行队列。

它可以保存：

- 曾经计划过的 submit/cancel/cancel-all。
- exchange 调用结果。
- 失败原因。
- superseded / abandoned / succeeded / failed 等历史状态。
- 创建时间、更新时间、session id。

它不能提供：

- `list_dispatchable_effects()`
- 当前 session 的派发顺序控制。
- 启动恢复输入。
- runtime 正确性判定。

## 6. 新抽象

### 6.1 `SessionEffectQueue`

`SessionEffectQueue` 是当前进程内的 effect 运行队列。

职责：

- 接收当前 mutation 产生的新 effect batch。
- 保持同一 batch 内的顺序。
- 只派发当前 batch 中已经解锁的 effect。
- 按 track 独立调度 effect；一个 track 的 `Deferred` / `Blocked` 不能阻塞其他 track。
- 拥有同一 batch 内 cancel 与 downstream submit 的解锁/废弃规则。
- 支持 effect worker 把 effect 标记为：
  - `Finished`：当前 effect 生命周期结束，后续 effect 可以继续。
  - `Deferred`：暂不执行，保留在同 track 队列中等待显式 wake；不立即重试，也不阻塞其他 track。
  - `Blocked`：当前 effect 失败并终结当前 batch 的剩余 effect，但不阻塞后续新 batch。
  - `Superseded`：当前 effect 被新 runtime 状态废弃，后续按 batch 规则继续。
- 提供当前 session 的 active submit hints，供 exchange sync 使用；这些 hint 只描述已经进入交易所交互窗口的 submit，不包含未来 queued submit。
- 提供当前 session 的 follow-up retirement token 和处理动作。

不负责：

- 写数据库。
- 跨重启恢复。
- UI 历史展示。
- 推导业务账务。

建议接口：

```rust
pub struct SessionEffectQueue { /* private */ }

pub enum SessionEffectOutcome {
    Finished,
    Superseded,
    Deferred { until: DeferredUntil },
    Blocked { reason: String },
}

pub enum DeferredUntil {
    FreshMarket,
    ExchangeState,
}

pub enum WakeSignal {
    FreshMarket,
    ExchangeState,
}

pub enum CancelReceiptResolution {
    ClosedWithoutFill,
    ClosedWithFill { filled_qty: f64 },
    StillWorking,
    Unknown { reason: String },
}

pub enum CancelQueueAction {
    UnblockedDownstream,
    SupersededDownstream {
        effect_ids: Vec<String>,
        requires_reconcile: bool,
    },
    Deferred { until: DeferredUntil },
    AwaitingFollowUpRetirement {
        reason: String,
        token: FollowUpRetirementToken,
    },
    Blocked { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FollowUpRetirementToken { /* opaque */ }

pub struct FollowUpRetirementResolution {
    pub token: FollowUpRetirementToken,
    pub closed_order_id: String,
}

pub enum FollowUpQueueAction {
    SupersededDownstream {
        effect_ids: Vec<String>,
        requires_reconcile: bool,
    },
    NothingToRetire,
    Blocked { reason: String },
}

pub enum SessionQueueAction {
    Continue,
    RetiredBatch {
        effect_ids: Vec<String>,
        requires_reconcile: bool,
    },
}

impl SessionEffectQueue {
    pub fn enqueue_batch(&self, effects: Vec<SessionTrackEffect>);
    pub fn claim_next(&self) -> Option<SessionTrackEffect>;
    pub fn record_submit_exchange_accepted(&self, effect_id: &str) -> bool;
    pub fn wake_track_for(&self, track_id: &TrackId, signal: WakeSignal);
    pub fn record_outcome(&self, effect_id: &str, outcome: SessionEffectOutcome) -> SessionQueueAction;
    pub fn record_cancel_resolution(
        &self,
        effect_id: &str,
        resolution: CancelReceiptResolution,
    ) -> CancelQueueAction;
    pub fn record_follow_up_retirement(
        &self,
        resolution: FollowUpRetirementResolution,
    ) -> FollowUpQueueAction;
    pub fn active_submit_hints_for_track(&self, track_id: &TrackId) -> Vec<PendingSubmitHint>;
    pub fn snapshot_for_track(&self, track_id: &TrackId) -> SessionEffectQueueSnapshot;
    pub fn clear_track(&self, track_id: &TrackId);
}
```

`CancelReceiptResolution` 的含义：

- `ClosedWithoutFill`：cancel 已终结且没有新增成交；同 batch 后续 submit 可以继续。
- `ClosedWithFill`：cancel 已终结但产生了部分或完全成交；同 batch 后续 submit 必须废弃，runtime 需要基于新 exposure 重新 reconcile。
- `StillWorking`：交易所回执显示订单仍可能工作；当前 cancel effect 不释放 downstream submit。
- `Unknown`：无法确认 cancel 结果；保留 downstream submit 的解释权在 queue 内，返回 follow-up token 并触发 bounded open-order sync。

这个分类由 application 层在吸收 exchange receipt 后产生，不能由 effect worker 仅凭 `Ok(receipt)` 推断。worker 只负责执行交易所调用并把回执交给 application；application 更新 manager 后返回分类；queue 根据分类一次性更新 cancel effect 和 downstream submit。

`SessionEffectOutcome::Deferred { until }` 的语义是“当前 track 需要等待某类 fresh input 后再尝试”。queue 不能把 deferred effect 留在全局队头反复 claim；它必须只暂停该 track 的当前 batch，并记录 `DeferredUntil`。只有匹配的 `wake_track_for(track_id, signal)` 才能让该 track 重新进入可派发集合。market freshness defer 只能被 `WakeSignal::FreshMarket` 唤醒；exchange state / cancel still-working defer 只能被 `WakeSignal::ExchangeState` 唤醒。这样普通行情 tick 不会重复唤醒仍在等待交易所订单状态的 cancel，也不会让不相关的 exchange sync 唤醒只缺行情 freshness 的 submit。

`SessionEffectOutcome::Blocked` 的语义是“当前 batch 已不能继续”，不是“整个 queue 停止”。queue 必须终结或跳过当前 batch 的剩余 effect，并允许同 track 后续新 batch 或其他 track 的 batch 继续派发；否则一次 cancel/submit failure 会把当前 session effect worker 卡死。

unknown cancel 不是普通 `Blocked`。普通 `Blocked` 表示 batch 失败并终结；unknown cancel 表示 downstream submit 暂时等待 bounded open-order sync 判断。queue 内部生成 `FollowUpRetirementToken`，并通过 `CancelQueueAction::AwaitingFollowUpRetirement { token, .. }` 返回给 application。后续 bounded open-order sync 只把 `token + closed_order_id` 交回 `record_follow_up_retirement(...)`；queue 内部用 token 找到当时被阻塞的 batch 和 downstream submit。调用层不能构造 batch 顺序身份，也不能查询 downstream 列表。

`record_submit_exchange_accepted(effect_id)` 是 queue 拥有 submit dispatch progress 的领域入口。worker 在交易所 `submit_order` 返回成功后、application writeback 之前调用它，把对应 submit effect 从 `InFlight` 推进到 `SubmittedAwaitingWriteback`。这样 exchange sync 可以看到“交易所已经接受、但本地 runtime 还没完成写回”的 submit hint，而调用方不需要也不能直接改 queue 内部状态。

`active_submit_hints_for_track(...)` 只暴露已经进入交易所交互窗口的 submit hint，例如 `InFlight` 或 `SubmittedAwaitingWriteback` 的 submit effect。它不能返回仍在 queue 中等待前置 cancel 释放、尚未 claim、尚未发往交易所的 downstream submit。未派发的 queued submit 是 queue 的计划状态，不是 exchange sync 的事实输入。

`SessionEffectQueue` 可以复用 `TrackEffect`，但不应叫 `PersistedTrackEffect`。第一阶段可引入：

```rust
pub struct SessionTrackEffect {
    pub effect_id: String,
    pub track_id: TrackId,
    pub batch_id: String,
    pub sequence: u32,
    pub effect: TrackEffect,
    pub created_at: DateTime<Utc>,
}
```

`SessionTrackEffect` 是 queue 的运行表示，不应作为 journal 的公开输入类型。

UI / read model 也不应直接消费 `SessionTrackEffect`。如果需要展示当前 session queue，queue 提供独立只读 DTO：

```rust
pub struct SessionEffectQueueSnapshot {
    pub track_id: TrackId,
    pub pending_effects: Vec<SessionPendingEffectView>,
}

pub struct SessionPendingEffectView {
    pub effect_id: String,
    pub kind: SessionPendingEffectKind,
    pub state: SessionPendingEffectState,
    pub created_at: DateTime<Utc>,
}

pub enum SessionPendingEffectKind {
    Submit,
    Cancel,
    Other,
}

pub enum SessionPendingEffectState {
    Queued,
    InFlight,
    SubmittedAwaitingWriteback,
    Deferred { until: DeferredUntil },
    AwaitingFollowUp,
}
```

这个 DTO 只表达展示需要的稳定语义，不暴露 `batch_id` / `sequence` / downstream 规则。queue 内部顺序模型变化时，TUI 和 HTTP read model 不需要跟着改。

### 6.2 `EffectJournal`

`EffectJournal` 是诊断记录接口。

职责：

- 记录当前 session 生成的 effect。
- 记录 effect 的最终历史状态。
- 支持 read model 查询最近 effect。

不负责：

- 派发 effect。
- 返回 dispatchable effect。
- 参与 startup 清理。
- 参与 exchange sync。

建议接口：

```rust
#[async_trait]
pub trait TrackEffectJournal: Send + Sync {
    async fn append_entries(&self, entries: &[EffectJournalEntry]) -> Result<()>;
    async fn record_effect_outcomes(&self, outcomes: &[EffectJournalOutcome]) -> Result<()>;
    async fn list_recent_track_effects(
        &self,
        track_id: &TrackId,
        limit: usize,
    ) -> Result<Vec<PersistedTrackEffect>>;
}
```

`EffectJournalEntry` 是 journal 自己的诊断输入类型。它可以包含 `effect_id`、`track_id`、`session_id`、`batch_id`、`sequence`、`TrackEffect`、创建时间等调试字段，但不应复用 `SessionTrackEffect`。如果 queue 的内部顺序模型未来改变，只需要修改一次从 queue effect 到 journal entry 的转换，不需要改 journal trait 和所有测试 fixture。

第一阶段可以保留 `track_effects` 表作为 journal 表，但要改名语义：

- 代码层不再叫 `TrackEffectStore`。
- `TrackMutationStore` 不再暴露 `update_effect_status` / `update_effect_statuses`。
- 查询层不再暴露 pending / dispatchable。
- worker 不再从该表读可执行工作。

## 7. Mutation 写边界

当前 `commit_track_transition(...)` 同时写业务状态、事件、effect，并返回 `CommittedTrackWrite.effects`。

新边界：

1. mutation 成功后，业务真值和 ledger events 原子提交。
2. mutation 产生的 effect 进入 `SessionEffectQueue`。
3. effect journal 通过 `TrackEffectJournal` 记录诊断历史；journal 失败不应让 runtime 回滚到旧状态。

原因：

- effect queue 属于当前 session runtime，不属于业务真值。
- 业务真值不能因为诊断日志失败而回滚。
- 重启后不 replay effect，因此 effect journal 不需要作为可靠 outbox。

目标接口：

- `TrackMutationStore::commit_track_transition(...)` 只提交 control state、ledger state 和 domain events。
- `SessionEffectQueue::enqueue_batch(...)` 接收当前 session effects。
- `TrackEffectJournal::append_entries(...)` 记录诊断历史。
- `TrackEffectJournal::record_effect_outcomes(...)` 记录执行结果，但不参与 runtime 正确性。

如果实现需要分阶段迁移，允许短期保留旧表和旧字段；但新 plan 不应继续把 effect status 写入放在 `TrackMutationStore` 上，也不应让任何运行路径读取 journal 的 pending 状态。

## 8. Effect worker

新 worker 模型：

```text
SessionEffectQueue.claim_next()
  -> execute effect
  -> for accepted submit: SessionEffectQueue.record_submit_exchange_accepted(...)
  -> write runtime result through application service
  -> application classifies cancel receipt when effect is cancel
  -> SessionEffectQueue.record_outcome(...) / record_cancel_resolution(...) / record_follow_up_retirement(...)
  -> EffectJournal.record_effect_outcomes(...)
```

worker 不再调用：

- `TrackEffectStore::list_dispatchable_effects()`
- `TrackEffectStore::list_pending_submit_effects_for_track()`
- `TrackEffectStore::list_pending_submit_effects_for_track_batch()`

如果 effect 需要先 reconcile：

- worker 返回 `Deferred { until: DeferredUntil::ExchangeState }`。
- queue 暂停该 track 的当前 batch。
- 其他 track 的 ready batch 继续派发。
- reconcile 完成后调用 `wake_track_for(track_id, WakeSignal::ExchangeState)`，该 track 才重新参与 claim。

`wake_track_for(track_id, signal)` 的 owner 是产生 fresh runtime 输入的 application/service 层，而不是 effect worker 自己。成功的 market observation 只能发送 `WakeSignal::FreshMarket`；exchange reconcile / open-order sync 只能发送 `WakeSignal::ExchangeState`。queue 只表达“这个 track 在等哪类新输入”，不需要知道新输入来自 websocket、REST sync 还是人工 reconcile。

如果 effect 调用失败且已经写入 runtime failure：

- worker 返回 `Blocked`。
- queue 终结当前 batch 的剩余 effect，并返回 `SessionQueueAction::RetiredBatch`。
- 后续新 batch 不被旧 blocked batch 阻塞。
- journal 记录当前 effect failed 和剩余 effect retired；这些记录是诊断历史，不参与运行协调。

如果 cancel 回执携带成交：

- application 先把回执吸收到 manager，计算 `CancelReceiptResolution::ClosedWithFill`。
- queue 将同 batch downstream submit 标记为 `Superseded`。
- runtime 触发 immediate reconcile，由新 exposure 重新规划。
- journal 只记录 cancel succeeded 与 downstream superseded 的历史，不承担原子运行协调。

## 9. Startup

startup 不再处理旧 effect queue。

保留流程：

1. cleanup inherited open orders。
2. 查询 position / open orders / rules。
3. 读取 `TrackControlState` 和 `TrackLedgerState`。
4. fresh-start runtime。
5. 清空当前进程内 `SessionEffectQueue` 中对应 track 的状态。
6. 开始 steady-state。

删除流程：

- 查询旧 `Pending / Executing` effect。
- 启动时把旧 effect 标为 `Superseded` 来保证 runtime 正确性。
- 查询并删除旧 follow-up retirement 来保证 runtime 正确性。

如果 UI 不希望显示旧 pending effect，可以在 journal 层用 session id 表示旧 session 已结束，而不是让 startup 以 runtime 清理的名义修改它。

## 10. Follow-up retirement

Follow-up retirement 是当前 session 中处理 unknown cancel outcome 的临时任务。

新语义：

- 存放在 `SessionEffectQueue` 或独立 `SessionFollowUpRetirementQueue`。
- 由 queue 生成不透明 `FollowUpRetirementToken`，并根据 token 解析、废弃对应 downstream submit。
- application 只提交 `FollowUpRetirementResolution { token, closed_order_id }`，不携带 `batch_id`、`sequence` 或 downstream 列表。
- application / worker 只处理 `FollowUpQueueAction`，不读取 batch 内部顺序。
- 只在当前进程内有效。
- 重启后不恢复。
- 重启后通过交易所 open orders 和 position 重建 runtime，自然纠正旧未知结果。

持久表 `follow_up_retirements` 应删除，或降级成诊断历史。

## 11. Read model / TUI

TUI 可以继续显示最近 effect，但必须表达清楚：

- effect 是历史记录。
- 旧 session 的 pending/executing effect 不代表当前还会执行。
- 当前实际运行状态来自 live runtime bindings、position、open orders 和 ledger。

建议展示：

- 当前 session pending effects：来自 `SessionEffectQueue` 的只读快照。
- 历史 effects：来自 `EffectJournal`。
- 旧 session 未终态 effect：显示为 `abandoned` 或 `previous session`，不显示成当前 pending。

## 12. 验收标准

### 12.1 重启不 replay 旧 effect

给定数据库里有旧 `Pending SubmitOrder`，启动新进程并完成 fresh-session 后：

- effect worker 不会执行旧 submit。
- exchange mock 的 submit 调用次数为 0。
- runtime 根据交易所当前 position/open orders 重新规划。

### 12.2 当前 session effect 仍会执行

给定当前 session 中 market tick 触发新 effect：

- effect 被放入 `SessionEffectQueue`。
- worker 能 claim 并执行该 effect。
- 成功后 queue 不再返回该 effect。
- journal 可查询到该 effect 的历史状态。

### 12.3 follow-up retirement 不跨重启

给定当前 session 中产生 follow-up retirement：

- 它能在当前 session 内清理 downstream submit。
- 重启后不会从持久层恢复该任务。
- fresh-session 通过交易所 open orders/position 得到正确 runtime。

### 12.4 cancel 带成交不会释放旧 downstream submit

给定当前 session 中 cancel-pending order 的 cancel 回执带 `filled_qty > 0` 或 manager 判断 fill progress 增加：

- cancel receipt 被分类为 `ClosedWithFill`。
- 同 batch 里 cancel 后面的 pending submit 被 session queue 标记为 `Superseded`。
- worker 不会继续派发这些 downstream submit。
- runtime 触发 reconcile，用最新 exposure 重新规划。

### 12.5 startup 不依赖 effect journal

`complete_startup` 和 `prepare_fresh_session_for_activation` 不调用：

- `list_session_reset_effects_for_track`
- `update_effect_status` / `update_effect_statuses` 来 supersede 旧 session work
- `list_follow_up_retirement_requests`
- `delete_follow_up_retirement_request`

### 12.6 账务持久化保留

重启后仍能恢复：

- 累计 realized PnL。
- 当日 fee/funding。
- unresolved ledger gaps。
- 用户控制状态。

## 13. 设计取舍

### 方案 A：继续使用持久 outbox，但加 session id

优点：

- 改动小。
- 保留现有 DB batch 顺序查询。

缺点：

- 运行正确性仍依赖 DB。
- 仍会让读者以为 effect 是恢复协议。
- pending/status 原子性问题继续存在。

结论：不采用作为目标设计。可以作为短期过渡，但不应作为最终架构。

### 方案 B：内存 session queue + 持久 journal

优点：

- 完全符合 fresh-session 共识。
- 运行队列与历史记录分离。
- 删除 startup 清旧 effect 的复杂度。
- 删除 DB pending dispatch 查询和 status-only 原子性压力。

缺点：

- 需要替换 effect worker 的消费源。
- 需要为当前 session active submit hints 提供新来源。

结论：采用。

### 方案 C：完全删除 effect 持久化

优点：

- 最简单。

缺点：

- 失去 TUI/HTTP 调试历史。
- 难以排查实际交易问题。

结论：不采用。保留 journal，但不让 journal 参与运行正确性。
