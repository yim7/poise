# Session-Scoped Effect Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 effect 从持久 outbox 改成当前进程内的 session queue，并把数据库 effect 数据降级成只服务 UI/诊断的 journal。

**Architecture:** 当前 runtime 正确性不再依赖 `track_effects`。`MutationExecutor` 在当前 session 产生 effect 后 enqueue 到 `SessionEffectQueue`，effect worker 只从这个内存队列 claim work；SQLite 继续记录 effect journal，但不提供 dispatchable/pending 恢复入口。Fresh-session 启动只基于配置、交易所 position/open orders/rules、控制状态和账务状态重建 runtime。

**Tech Stack:** Rust workspace, Tokio, Cargo, SQLite, Serde, chrono

---

## 设计约束

- 重启后不 replay 旧 effect。
- 运行环境不依赖 `track_effects`。
- 持久化保留账务、交易历史、控制状态和诊断 journal。
- effect worker 不再调用旧 effect store/journal 的 `list_dispatchable_effects()`。
- startup 不再扫描旧 `Pending / Executing` effect 来保证 runtime 正确性。
- cancel follow-up 是当前 session 临时任务，不跨重启。

## 文件与责任

- Create: `application/src/session_effect_queue.rs`
  - owner 当前进程内 effect batch 队列
  - owner per-track dispatch claim、defer、wake、finish、block 语义
  - owner cancel receipt 对同 batch downstream submit 的解锁/废弃规则
  - owner 当前 session active submit hints 和 cancel follow-up 领域动作
- Modify: `application/src/lib.rs`
  - 导出 `SessionEffectQueue`、`SessionTrackEffect`、`SessionEffectOutcome`
- Modify: `application/src/track_persistence.rs`
  - 保留 `PersistedTrackEffect` 作为 journal/read model 类型
  - 增加从当前 session effect 到 `EffectJournalEntry` / journal row 的转换辅助
- Modify: `application/src/track_mutation_store.rs`
  - 只保留业务真值、ledger events 的持久写入语义
  - 删除 effect status / journal outcome 写入语义
- Modify: `application/src/track_effect_store.rs`
  - 从第一个共享边界任务开始改名为 `TrackEffectJournal`
  - 删除 `list_dispatchable_effects`、pending batch 查询、session reset 查询等运行队列接口
  - owner effect journal append/outcome/read 语义，且 outcome 写入为 best-effort diagnostic
- Modify: `application/src/mutation_executor.rs`
  - 注入 `SessionEffectQueue`
  - mutation 持久化成功后 enqueue 当前 session effects
  - exchange sync active submit hints 改从 queue 读取
  - cancel receipt 分类后由 queue 统一处理 downstream submit
  - cancel follow-up 通过 queue action 处理，不直接读取或构造 batch/sequence downstream 身份
  - cancel follow-up 改为 session 内存状态
  - `prepare_fresh_session_for_activation` 不再清旧 DB effect
- Modify: `application/src/runtime_lifecycle_service.rs`
  - fresh-session activation 只清当前 session queue/runtime，不清 DB pending effect
- Modify: `application/src/submit_effect_service.rs`
  - submit recovery 只针对当前 session queue item
- Modify: `server/src/effect_worker/dispatch.rs`
  - worker 从 `SessionEffectQueue` claim work
  - 不再扫 DB pending effects
- Modify: `server/src/effect_worker/execute.rs`
  - effect 执行后返回 queue outcome
  - freshness gate defer 时返回 `Deferred`
  - 已记录失败时返回 `Blocked`
- Modify: `server/src/effect_worker/mod.rs`
  - 持有 `SessionEffectQueue`
- Modify: `server/src/server_context.rs`
  - `ReconcileState` / `EffectWorkerState` 持有 queue，而不是把 effect store 当 runtime queue
- Modify: `server/src/assembly.rs`
  - 创建并注入单例 `SessionEffectQueue`
- Modify: `server/src/runtime/startup_bootstrap.rs`
  - 删除 startup 对旧 effect/cancel follow-up 的清理依赖
- Modify: `storage/src/sqlite.rs`
  - `track_effects` 只作为 journal 查询和记录
  - 删除或停止使用 dispatchable/pending runtime 查询
- Modify: `storage/src/schema.rs`
  - 保留 `track_effects` journal 表
  - 删除 pending dispatch 索引，或改成 recent journal 查询索引
- Modify: `application/src/read_model.rs`
  - 区分当前 session queue 和历史 journal
- Modify: `server/src/projector.rs`
  - TUI/HTTP 对旧 session pending effect 显示为历史，不显示为当前待执行
- Modify: `docs/superpowers/specs/2026-04-25-session-effect-queue-and-journal-design.md`
  - 实现过程中如果接口名变化，回写 spec

## Task 1: 建立第一个可运行垂直切片

本 task 必须同时完成四件事：

1. journal 降级为诊断边界，旧 DB pending effect 不再被派发。
2. 引入 `SessionEffectQueue`。
3. mutation 成功后 enqueue 当前 session effect。
4. effect worker 从 `SessionEffectQueue` claim 当前 session work。

这四件事是一个任务边界，不能拆成多个提交。否则会出现“旧持久 outbox 已删除、新 session queue 还没接上”的运行空窗。

执行记录：

- 2026-04-25：完成第一个可运行切片，commit `3edc094`。
- 验收：`cargo test -p poise-application session_effect_queue -- --nocapture`
- 验收：`cargo test -p poise-application mutation_executor::tests:: -- --nocapture`
- 验收：`cargo test -p poise-server effect_worker:: -- --nocapture`

### Task 1A: 建立 journal 边界，并锁住“旧 DB pending effect 不会被派发”

**Files:**

- Modify: `server/src/effect_worker/dispatch.rs`
- Modify: `application/src/track_effect_store.rs`
- Modify: `application/src/track_mutation_store.rs`
- Modify: `storage/src/sqlite.rs`
- Test: `server/src/effect_worker/dispatch.rs`
- Test Support: `server/src/test_support.rs`

- [ ] **Step 1: 写失败测试**

在 `server/src/effect_worker/dispatch.rs` 的 test module 中添加测试。若当前文件没有 test module，就在文件末尾创建：

```rust
#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use poise_storage::sqlite::SqliteStorage;

    use crate::effect_worker::EffectWorker;
    use crate::test_support::{
        NoopAccountPort, RecordingExecutionPort, build_effect_worker_context_for_repository,
        seed_persisted_pending_submit_effect,
    };

    #[tokio::test]
    async fn worker_does_not_dispatch_persisted_effects_from_previous_session() {
        let repository = Arc::new(SqliteStorage::in_memory().unwrap());
        seed_persisted_pending_submit_effect(repository.as_ref(), "btc-core")
            .await
            .unwrap();

        let effect_worker_context = build_effect_worker_context_for_repository(repository);
        let execution = Arc::new(RecordingExecutionPort::default());
        let account = Arc::new(NoopAccountPort::default());
        let worker = EffectWorker::new(
            effect_worker_context,
            execution.clone(),
            account,
            Duration::from_millis(1),
        );

        worker.run_once().await.unwrap();

        assert_eq!(
            execution.submit_order_call_count(),
            0,
            "effect worker must not dispatch persisted pending effects from a previous session"
        );
    }
}
```

在同一个 task 内为 `server/src/test_support.rs` 增加专用测试支撑。不要从 `poise_application::mutation_executor::test_support` 引入私有 helper；server 测试只复用 server 自己的 test support。

先把共享边界改到最终命名，避免计划先强化旧接口再回头拆：

```rust
pub struct EffectJournalEntry {
    pub effect_id: String,
    pub track_id: TrackId,
    pub session_id: String,
    pub batch_id: String,
    pub sequence: u32,
    pub effect: TrackEffect,
    pub created_at: DateTime<Utc>,
}

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

同步删除 `TrackMutationStore::commit_track_transition(...)` 的 `effects` / `effect_status_updates` 参数，以及 `update_effect_status(...)` / `update_effect_statuses(...)`。这一步必须和本 task 的 worker / journal 垂直切片一起做到测试可通过，不允许提交共享接口半迁移状态，也不要添加临时 adapter。

先增加一个可复用 manager / worker context helper，后续 queue 测试也会用它：

```rust
pub(crate) fn test_manager(track_id: &str) -> poise_engine::manager::TrackManager {
    let mut manager = poise_engine::manager::TrackManager::new(Arc::new(crate::assembly::SystemClock));
    manager
        .add_track(
            TrackId::new(track_id),
            poise_engine::track::Instrument::new(Venue::Binance, default_symbol_for(track_id)),
            poise_core::strategy::TrackConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: poise_core::strategy::ShapeFamily::Linear,
                out_of_band_policy: poise_core::strategy::BandProtectionPolicy::Freeze,
            },
            test_max_notional(),
            test_loss_limits(),
            poise_core::types::ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.1,
                min_qty: 0.0,
                min_notional: 0.0,
                maker_fee_rate: 0.0,
                taker_fee_rate: 0.0,
            },
        )
        .unwrap();
    manager
}

pub(crate) fn build_effect_worker_context_for_repository<R>(
    repository: Arc<R>,
) -> EffectWorkerTestContext
where
    R: TrackMutationStore + TrackQueryStore + TrackEffectJournal + 'static,
{
    let (notifications, _) = tokio::sync::broadcast::channel(16);
    let account_margin_guard = Arc::new(AccountMarginGuardStore::default());
    let mutation_store = repository.clone() as Arc<dyn TrackMutationStore>;
    let query_store = repository.clone() as Arc<dyn TrackQueryStore>;
    let effect_journal = repository as Arc<dyn TrackEffectJournal>;
    let services = build_test_application_services(
        test_manager("btc-core"),
        mutation_store,
        query_store.clone(),
        effect_journal.clone(),
        notifications,
        account_margin_guard,
    );
    build_effect_worker_test_context(&services, query_store, effect_journal)
}
```

再增加 seed helper。它通过正式 journal 接口写入一条旧 session 的 pending submit journal row，而不是手工插 SQL，也不是通过 `TrackMutationStore` 写 effect：

```rust
pub(crate) async fn seed_persisted_pending_submit_effect(
    journal: &dyn TrackEffectJournal,
    track_id: &str,
) -> anyhow::Result<()> {
    journal
        .append_entries(&[EffectJournalEntry {
            effect_id: format!("{track_id}:old-session:0"),
            track_id: TrackId::new(track_id),
            session_id: "old-session".into(),
            batch_id: "old-session".into(),
            sequence: 0,
            effect: test_submit_effect(track_id),
            created_at: chrono::Utc::now(),
        }])
        .await?;
    Ok(())
}
```

同一段 test support 里补一个最小 submit effect 构造器，避免测试依赖 application crate 的私有 helper：

```rust
pub(crate) fn test_submit_effect(track_id: &str) -> poise_engine::transition::TrackEffect {
    poise_engine::transition::TrackEffect::SubmitOrder {
        request: poise_engine::ports::OrderRequest {
            instrument: poise_engine::track::Instrument::new(
                poise_engine::track::Venue::Binance,
                default_symbol_for(track_id),
            ),
            side: poise_core::types::Side::Buy,
            price: 100.0,
            quantity: 0.1,
            client_order_id: format!("{track_id}-old-session-submit"),
            reduce_only: false,
        },
        desired_exposure: poise_core::types::Exposure(4.0),
        submit_purpose: poise_engine::price_gate::SubmitPurpose::AutoReconcile,
        recovery_token: poise_engine::executor::SubmitRecoveryToken::empty(),
    }
}
```

最后增加 `RecordingExecutionPort` 和 `NoopAccountPort`，测试不复用 assembly 私有 fake：

```rust
#[derive(Default)]
pub(crate) struct RecordingExecutionPort {
    submitted: std::sync::Mutex<Vec<poise_engine::ports::OrderRequest>>,
}

impl RecordingExecutionPort {
    pub(crate) fn submit_order_call_count(&self) -> usize {
        self.submitted.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl poise_engine::ports::ExecutionPort for RecordingExecutionPort {
    async fn submit_order(
        &self,
        req: poise_engine::ports::OrderRequest,
    ) -> anyhow::Result<poise_engine::ports::OrderReceipt> {
        self.submitted.lock().unwrap().push(req.clone());
        Ok(poise_engine::ports::OrderReceipt {
            order_id: "test-order".into(),
            client_order_id: req.client_order_id,
            filled_qty: 0.0,
            status: poise_engine::ports::OrderStatus::New,
        })
    }

    async fn cancel_order(
        &self,
        _instrument: &poise_engine::track::Instrument,
        order_id: &str,
    ) -> anyhow::Result<poise_engine::ports::OrderReceipt> {
        Ok(poise_engine::ports::OrderReceipt {
            order_id: order_id.to_string(),
            client_order_id: String::new(),
            filled_qty: 0.0,
            status: poise_engine::ports::OrderStatus::Canceled,
        })
    }

    async fn cancel_all(
        &self,
        _instrument: &poise_engine::track::Instrument,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get_position(
        &self,
        instrument: &poise_engine::track::Instrument,
    ) -> anyhow::Result<poise_engine::ports::Position> {
        Ok(poise_engine::ports::Position {
            instrument: instrument.clone(),
            qty: 0.0,
            avg_price: 0.0,
            unrealized_pnl: 0.0,
        })
    }

    async fn get_open_orders(
        &self,
        _instrument: &poise_engine::track::Instrument,
    ) -> anyhow::Result<poise_engine::ports::ExchangeOpenOrderSnapshot> {
        Ok(poise_engine::ports::ExchangeOpenOrderSnapshot::from_complete_exchange_query(
            Vec::new(),
        ))
    }
}

#[derive(Default)]
pub(crate) struct NoopAccountPort;

#[async_trait::async_trait]
impl poise_engine::ports::AccountPort for NoopAccountPort {
    async fn get_account_capacity_snapshot(
        &self,
        _instrument: &poise_engine::track::Instrument,
    ) -> anyhow::Result<poise_engine::ports::AccountCapacitySnapshot> {
        Ok(poise_engine::ports::AccountCapacitySnapshot {
            max_increase_notional: 1_000_000.0,
        })
    }

    async fn subscribe_user_data(
        &self,
    ) -> anyhow::Result<tokio::sync::mpsc::Receiver<poise_engine::ports::UserDataEvent>> {
        let (_sender, receiver) = tokio::sync::mpsc::channel(1);
        Ok(receiver)
    }
}
```

在同一个 test module 里定义 `cancel_effect(...)` 和 `submit_effect(...)` helper，使用真实 `TrackEffect::CancelOrder` / `TrackEffect::SubmitOrder`，不要用伪 enum 代替业务 effect。

- [ ] **Step 2: 运行测试确认失败**

Run:

```bash
cargo test -p poise-server worker_does_not_dispatch_persisted_effects_from_previous_session -- --nocapture
```

Expected: FAIL。当前 worker 仍从 `list_dispatchable_effects()` 扫 DB，因此会调用 submit。

- [ ] **Step 3: 实现最小垂直切片并让测试通过**

本 task 不是“提交红测试”。它必须在同一个提交里完成以下最小切换：

1. `TrackEffectJournal` 能写入和查询最近 journal row。
2. `TrackMutationStore` 不再接收或更新 effect / effect status。
3. `EffectWorker::run_once()` 不再从 journal / DB 查询 dispatchable effect。
4. `worker_does_not_dispatch_persisted_effects_from_previous_session` 通过。

如果中途编译失败，继续修到上述最小切片通过后再提交；不要提交 RED 状态，也不要用临时双接口保留旧 outbox 语义。

Run:

```bash
cargo test -p poise-server worker_does_not_dispatch_persisted_effects_from_previous_session -- --nocapture
```

Expected: PASS。

- [ ] **Step 4: 阶段检查，不提交**

本阶段只是垂直切片的第一段。测试通过后继续 Task 1B，不要提交。

```bash
cargo test -p poise-server worker_does_not_dispatch_persisted_effects_from_previous_session -- --nocapture
```

### Task 1B: 引入 `SessionEffectQueue`

**Files:**

- Create: `application/src/session_effect_queue.rs`
- Modify: `application/src/lib.rs`
- Test: `application/src/session_effect_queue.rs`

- [ ] **Step 1: 写 queue 单元测试**

创建 `application/src/session_effect_queue.rs`，先写测试和类型骨架。下面的测试使用 `cancel_all()`、`cancel_order()`、`submit_order(...)` 这类本地 helper 创建 `TrackEffect`，不直接构造 `SessionTrackEffect`，也不传入 `batch_id` / `sequence`：

```rust
#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;

    use super::{
        CancelQueueAction, CancelReceiptResolution, DeferredUntil, SessionEffectOutcome,
        SessionEffectQueue, SessionPendingEffectState, SessionQueueAction, WakeSignal,
    };

    fn enqueue_effects(
        queue: &SessionEffectQueue,
        track_id: &str,
        effects: &[TrackEffect],
    ) -> Vec<String> {
        queue
            .enqueue_transition_effects(&TrackId::new(track_id), effects, Utc::now())
            .into_iter()
            .map(|effect| effect.effect_id)
            .collect()
    }

    #[test]
    fn queue_dispatches_batch_in_sequence_order() {
        let queue = SessionEffectQueue::default();
        let enqueued = enqueue_effects(&queue, "btc-core", &[cancel_all(), cancel_all()]);

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);
        queue.record_outcome(&enqueued[0], SessionEffectOutcome::Finished);
        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[1]);
    }

    #[test]
    fn blocked_effect_retires_current_batch_and_allows_next_batch() {
        let queue = SessionEffectQueue::default();
        let first = enqueue_effects(&queue, "btc-core", &[cancel_all(), cancel_all()]);
        let second = enqueue_effects(&queue, "btc-core", &[cancel_all()]);

        assert_eq!(queue.claim_next().unwrap().effect_id, first[0]);
        let action = queue.record_outcome(
            &first[0],
            SessionEffectOutcome::Blocked {
                reason: "cancel failed".to_string(),
            },
        );
        assert_eq!(
            action,
            SessionQueueAction::RetiredBatch {
                effect_ids: vec![first[1].clone()],
                requires_reconcile: true,
            }
        );
        assert_eq!(queue.claim_next().unwrap().effect_id, second[0]);
    }

    #[test]
    fn deferred_effect_blocks_only_its_track_until_matching_wake() {
        let queue = SessionEffectQueue::default();
        let btc = enqueue_effects(&queue, "btc-core", &[cancel_all()]);
        let eth = enqueue_effects(&queue, "eth-core", &[cancel_all()]);

        assert_eq!(queue.claim_next().unwrap().effect_id, btc[0]);
        queue.record_outcome(
            &btc[0],
            SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            },
        );
        assert_eq!(queue.claim_next().unwrap().effect_id, eth[0]);
        queue.record_outcome(&eth[0], SessionEffectOutcome::Finished);
        assert!(queue.claim_next().is_none());

        queue.wake_track_for(&TrackId::new("btc-core"), WakeSignal::FreshMarket);
        assert!(queue.claim_next().is_none());

        queue.wake_track_for(&TrackId::new("btc-core"), WakeSignal::ExchangeState);
        assert_eq!(queue.claim_next().unwrap().effect_id, btc[0]);
    }

    #[test]
    fn cancel_with_fill_supersedes_downstream_submit_effects() {
        let queue = SessionEffectQueue::default();
        let enqueued = enqueue_effects(
            &queue,
            "btc-core",
            &[cancel_order(), submit_order("client-1"), submit_order("client-2")],
        );

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);
        let action = queue.record_cancel_resolution(
            &enqueued[0],
            CancelReceiptResolution::ClosedWithFill { filled_qty: 0.4 },
        );

        assert_eq!(
            action,
            CancelQueueAction::SupersededDownstream {
                effect_ids: vec![enqueued[1].clone(), enqueued[2].clone()],
                requires_reconcile: true,
            }
        );
        assert!(queue.claim_next().is_none());
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run:

```bash
cargo test -p poise-application session_effect_queue -- --nocapture
```

Expected: FAIL，因为类型尚未实现。

- [ ] **Step 3: 实现最小 queue**

在 `application/src/session_effect_queue.rs` 添加实现。核心结构必须是 per-track queue，不要用单个全局队头承载所有 track：

```rust
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use poise_engine::track::TrackId;
use poise_engine::transition::TrackEffect;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionTrackEffect {
    pub effect_id: String,
    pub track_id: TrackId,
    pub effect: TrackEffect,
    pub created_at: DateTime<Utc>,
    pub(crate) batch_id: String,
    pub(crate) sequence: u32,
}

// batch_id / sequence 是 queue 内部诊断身份，只能由 SessionEffectQueue 生成。
// 调用方不能构造或依赖这些字段。

#[derive(Debug, Clone, PartialEq)]
pub struct SessionEffectQueueSnapshot {
    pub track_id: TrackId,
    pub pending_effects: Vec<SessionPendingEffectView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionPendingEffectView {
    pub effect_id: String,
    pub kind: SessionPendingEffectKind,
    pub state: SessionPendingEffectState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPendingEffectKind {
    Submit,
    Cancel,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPendingEffectState {
    Queued,
    InFlight,
    SubmittedAwaitingWriteback,
    Deferred { until: DeferredUntil },
    AwaitingFollowUp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEffectOutcome {
    Finished,
    Superseded,
    Deferred { until: DeferredUntil },
    Blocked { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredUntil {
    FreshMarket,
    ExchangeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeSignal {
    FreshMarket,
    ExchangeState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CancelReceiptResolution {
    ClosedWithoutFill,
    ClosedWithFill { filled_qty: f64 },
    StillWorking,
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CancelQueueAction {
    UnblockedDownstream,
    SupersededDownstream {
        effect_ids: Vec<String>,
        requires_reconcile: bool,
    },
    Deferred { until: DeferredUntil },
    AwaitingCancelFollowUp {
        reason: String,
    },
    Blocked { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum FollowUpQueueAction {
    SupersededDownstream {
        effect_ids: Vec<String>,
        requires_reconcile: bool,
    },
    StillWorking {
        order_id: String,
    },
    NothingToRetire,
    Blocked { reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionQueueAction {
    Continue,
    RetiredBatch {
        effect_ids: Vec<String>,
        requires_reconcile: bool,
    },
}

#[derive(Clone, Default)]
pub struct SessionEffectQueue {
    inner: Arc<Mutex<SessionEffectQueueInner>>,
}

#[derive(Default)]
struct SessionEffectQueueInner {
    tracks: HashMap<TrackId, TrackQueue>,
    ready_tracks: VecDeque<TrackId>,
    effect_index: HashMap<String, TrackId>,
    follow_up_tokens: HashMap<InternalFollowUpKey, FollowUpPointer>,
    next_batch_id: u64,
}

#[derive(Default)]
struct TrackQueue {
    batches: VecDeque<SessionEffectBatch>,
    paused_until: Option<DeferredUntil>,
    in_ready_ring: bool,
}

struct SessionEffectBatch {
    effects: VecDeque<QueuedEffect>,
}

struct QueuedEffect {
    effect: SessionTrackEffect,
    dispatch_state: QueuedEffectState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuedEffectState {
    Queued,
    InFlight,
    SubmittedAwaitingWriteback,
}

struct FollowUpPointer {
    track_id: TrackId,
    cancel_effect_id: String,
}
```

实现要求：

- `enqueue_transition_effects(track_id, effects, created_at)` 是 public enqueue 入口。调用方只提交领域 `TrackEffect` 列表；queue 内部生成 batch id、effect id 和 sequence，并把未暂停的 track 放入 `ready_tracks`。
- `claim_next()` 扫描 `ready_tracks`，选择第一个未暂停、队头 batch 有可执行 effect、且该 effect `dispatch_state == Queued` 的 track。claim 后把 effect 标记为 `InFlight`，再把 track 放到 ready ring 尾部，实现简单公平调度。
- `record_submit_exchange_accepted(effect_id)` 是 submit dispatch progress 的唯一公开入口。交易所 submit 调用成功但 application writeback 尚未完成时，worker 必须调用它，把对应 submit effect 从 `InFlight` 标记为 `SubmittedAwaitingWriteback`。这个状态只用于 exchange sync hint 和当前 queue 展示，writeback 完成后该 effect 仍按 outcome 从 queue 移除或进入下一状态。
- `record_submit_exchange_accepted(...)` 只接受当前 session queue 中已 claim 的 `SubmitOrder` effect。找不到 effect、effect 类型不是 submit、或状态不是 `InFlight` 时返回 `false`，worker 记录诊断日志并按异常路径处理；调用方不能绕过 queue 直接改 `dispatch_state`。
- `SessionEffectOutcome::Deferred { until }` 只把该 effect 所在 track 标记为 `paused_until = Some(until)`，不重新放入 ready ring；直到 `wake_track_for(track_id, matching_signal)` 被调用才允许重试。错误类型的 wake signal 必须保持 paused。
- `SessionEffectOutcome::Blocked` 只 retire 当前 batch 剩余 effect，并让同 track 后续 batch 和其他 track 继续参与调度。
- `CancelReceiptResolution::Unknown` 在 queue 内部创建 follow-up 指针，并返回 `CancelQueueAction::AwaitingCancelFollowUp { reason }`。调用方不能持有 token，也不能构造或读取 `batch_id` / `sequence`。
- `resolve_cancel_follow_ups_from_open_order_snapshot(...)` 是 cancel follow-up 的 public 入口。bounded open-order sync 完成后把完整 `CompleteOpenOrderSnapshot` 交给 queue，由 queue 内部根据 follow-up 指针解释订单已关闭或仍在 open orders 的结果，并返回 action。
- `active_submit_hints_for_track(...)` 只返回 `InFlight` / `SubmittedAwaitingWriteback` 的 submit effect；尚未 claim 的 queued downstream submit 不能进入 exchange sync hint。
- `snapshot_for_track(...)` 返回 `SessionEffectQueueSnapshot` 展示 DTO，只包含 `effect_id`、kind、state、created_at 等稳定展示字段；不能返回 `SessionTrackEffect`，也不能暴露 `batch_id` / `sequence`。

public impl 至少包含这些运行入口：

```rust
impl SessionEffectQueue {
    pub fn enqueue_transition_effects(
        &self,
        track_id: &TrackId,
        effects: &[TrackEffect],
        created_at: DateTime<Utc>,
    ) -> Vec<SessionTrackEffect>;
    pub fn claim_next(&self) -> Option<SessionTrackEffect>;
    pub fn record_submit_exchange_accepted(&self, effect_id: &str) -> bool;
    pub fn record_outcome(&self, effect_id: &str, outcome: SessionEffectOutcome) -> SessionQueueAction;
    pub fn record_cancel_resolution(
        &self,
        effect_id: &str,
        resolution: CancelReceiptResolution,
    ) -> CancelQueueAction;
    pub fn resolve_cancel_follow_ups_from_open_order_snapshot(
        &self,
        track_id: &TrackId,
        open_orders: &CompleteOpenOrderSnapshot,
    ) -> Vec<FollowUpQueueAction>;
    pub fn wake_track_for(&self, track_id: &TrackId, signal: WakeSignal);
    pub fn active_submit_hints_for_track(&self, track_id: &TrackId) -> Vec<PendingSubmitHint>;
    pub fn snapshot_for_track(&self, track_id: &TrackId) -> SessionEffectQueueSnapshot;
    pub fn clear_track(&self, track_id: &TrackId);
}
```

- [ ] **Step 4: 导出类型**

在 `application/src/lib.rs` 添加：

```rust
mod session_effect_queue;
pub use session_effect_queue::{
    CancelQueueAction, CancelReceiptResolution, DeferredUntil, FollowUpQueueAction,
    SessionEffectOutcome, SessionEffectQueue, SessionEffectQueueSnapshot,
    SessionPendingEffectKind, SessionPendingEffectState, SessionPendingEffectView,
    SessionQueueAction, SessionTrackEffect, WakeSignal,
};
```

- [ ] **Step 5: 运行测试确认通过**

Run:

```bash
cargo test -p poise-application session_effect_queue -- --nocapture
```

Expected: PASS。

- [ ] **Step 6: 阶段检查，不提交**

本阶段只是垂直切片的第二段。测试通过后继续 Task 1C，不要提交。

```bash
cargo test -p poise-application session_effect_queue -- --nocapture
```

### Task 1C: mutation 成功后 enqueue 当前 session effect

**Files:**

- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/track_effect_store.rs`
- Modify: `application/src/track_mutation_store.rs`
- Modify: `application/src/track_persistence.rs`
- Modify: `application/src/runtime_lifecycle_service.rs`
- Modify: `application/src/track_observation_service.rs`
- Modify: `application/src/track_command_service.rs`
- Modify: `storage/src/sqlite.rs`
- Test: `application/src/mutation_executor.rs`

- [ ] **Step 1: 写失败测试**

在 `application/src/mutation_executor.rs` 的 tests 中新增：

```rust
#[tokio::test]
async fn observe_market_enqueues_current_session_effects() {
    let repository = Arc::new(MemoryRepository::default());
    let (services, _) = track_write_services(seeded_manager(), repository);

    services
        .observation
        .observe_market(
            "btc-core",
            MarketObservation::ExecutionQuote {
                execution_quote: ExecutionQuote {
                    best_bid: 104.9,
                    best_ask: 105.1,
                },
            },
        )
        .await
        .unwrap();

    let next = services
        .session_effect_queue
        .claim_next()
        .expect("current session effect should be enqueued");

    assert_eq!(next.track_id, TrackId::new("btc-core"));
}
```

如果 `TrackServiceSet` 还不暴露 queue，先让测试引用失败。

- [ ] **Step 2: 运行测试确认失败**

Run:

```bash
cargo test -p poise-application observe_market_enqueues_current_session_effects -- --nocapture
```

Expected: FAIL，因为 `TrackServiceSet` 还没有 `session_effect_queue`。

- [ ] **Step 3: 注入 queue**

在 `MutationExecutor` 字段中添加：

```rust
session_effect_queue: SessionEffectQueue,
```

在构造函数中传入并保存：

```rust
pub(crate) fn new(
    manager: TrackManager,
    mutation_store: Arc<dyn TrackMutationStore>,
    effect_store: Arc<dyn TrackEffectJournal>,
    session_effect_queue: SessionEffectQueue,
    notifications: broadcast::Sender<ApplicationNotification>,
    account_margin_guard: Arc<dyn AccountCapacityGuard>,
    recovery_anomaly_observer: Arc<dyn RecoveryAnomalyObserver>,
) -> Self
```

同一 task 内把 `TrackMutationStore::commit_track_transition(...)` 收回到业务真值边界，不再接收 `effects` 和 `effect_status_updates`：

```rust
async fn commit_track_transition(
    &self,
    id: &str,
    control_state: Option<&TrackControlState>,
    ledger_state: &TrackLedgerState,
    events: &[DomainEvent],
) -> Result<CommittedTrackWrite>;
```

`CommittedTrackWrite` 不再返回 `effects`。本次 transition 的 `effects` 由 `MutationExecutor` 从内存里的 `TrackTransition` 直接构造 `SessionTrackEffect`，并转换成独立的 `EffectJournalEntry` 写入 journal。

同时把旧 `TrackEffectStore` 边界改成最小 `TrackEffectJournal` 接口，并让 `MutationExecutor` 只依赖这个 journal 接口写 effect 历史：

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

`EffectJournalEntry` 放在 journal / persistence 边界，不放在 `session_effect_queue.rs`。它是诊断输入 DTO；从当前 session effect 到 journal entry 的转换由应用层本地完成，journal trait 不直接依赖 `SessionTrackEffect`，`EffectJournalEntry` 也不反向知道 session queue 的运行类型。

在 mutation 业务持久化成功后，从本次 transition 产生的 `effects` 构造 session effects。不要从 `CommittedTrackWrite` 或 `PersistedTrackEffect` 反推当前 session work：

```rust
let _committed_write = persistence_result.map_err(|error| {
    TrackMutationError::Persistence(error)
})?;

let session_effects = self.session_effect_queue.enqueue_transition_effects(
    &TrackId::new(id),
    &effects,
    self.clock.now(),
);
let journal_entries = effect_journal_entries_from_session_effects(&session_effects);

if !session_effects.is_empty() {
    if let Err(error) = self.effect_journal.append_entries(&journal_entries).await {
        tracing::warn!(
            track_id = id,
            "failed to append effect journal entries: {error}"
        );
    }
}
```

`enqueue_transition_effects(...)` 在 `session_effect_queue.rs` 中实现。调用方不提供 `batch_id` / `sequence`；queue 内部生成当前 session 的 batch/effect identity，并返回已入队的 `SessionTrackEffect` 供应用层转换成诊断 journal entry：

```rust
impl SessionEffectQueue {
    pub fn enqueue_transition_effects(
        &self,
        track_id: &TrackId,
        effects: &[TrackEffect],
        created_at: DateTime<Utc>,
    ) -> Vec<SessionTrackEffect> {
        let batch_id = self.next_batch_id_for(track_id);
        let session_effects = effects
            .iter()
            .enumerate()
            .filter_map(|(sequence, effect)| {
                if matches!(effect, TrackEffect::NoOp) {
                    return None;
                }
                Some(SessionTrackEffect {
                    effect_id: format!("{}:{batch_id}:{sequence}", track_id.as_str()),
                    track_id: track_id.clone(),
                    effect: effect.clone(),
                    created_at,
                    batch_id: batch_id.clone(),
                    sequence: sequence as u32,
                })
            })
            .collect::<Vec<_>>();
        self.enqueue_prepared_effects(session_effects.clone());
        session_effects
    }
}
```

`enqueue_prepared_effects(...)` 如果保留，只能是 queue 模块内部 helper，不能作为跨模块 public 入口：

```rust
fn enqueue_prepared_effects(
    &self,
    effects: Vec<SessionTrackEffect>,
);
```

- [ ] **Step 4: 在 TrackServiceSet 创建共享 queue**

在 `TrackServiceSet` 中添加字段：

```rust
pub session_effect_queue: SessionEffectQueue,
```

在 `TrackServiceSet::new_with_recovery_anomaly_observer(...)` 中创建：

```rust
let session_effect_queue = SessionEffectQueue::default();
let executor = Arc::new(MutationExecutor::new(
    manager,
    mutation_store,
    effect_journal,
    session_effect_queue.clone(),
    notifications.clone(),
    account_margin_guard,
    recovery_anomaly_observer,
));
```

- [ ] **Step 5: 运行测试确认通过**

Run:

```bash
cargo test -p poise-application observe_market_enqueues_current_session_effects -- --nocapture
```

Expected: PASS。

- [ ] **Step 6: 阶段检查，不提交**

本阶段只是垂直切片的第三段。测试通过后继续 Task 1D，不要提交。

```bash
cargo test -p poise-application observe_market_enqueues_current_session_effects -- --nocapture
```

### Task 1D: effect worker 改为消费 `SessionEffectQueue`

**Files:**

- Modify: `server/src/effect_worker/mod.rs`
- Modify: `server/src/effect_worker/dispatch.rs`
- Modify: `server/src/effect_worker/execute.rs`
- Modify: `server/src/server_context.rs`
- Modify: `server/src/assembly.rs`
- Modify: `application/src/track_observation_service.rs`
- Modify: `server/src/runtime/reconcile.rs`
- Test: `server/src/effect_worker/dispatch.rs`

- [ ] **Step 1: 写当前 session effect 会执行的测试**

在 `server/src/effect_worker/dispatch.rs` tests 中新增：

```rust
#[tokio::test]
async fn worker_dispatches_current_session_queue_effect() {
    let repository = Arc::new(SqliteStorage::in_memory().unwrap());
    let (effect_worker_context, queue) =
        build_effect_worker_context_for_repository_with_queue(repository);
    queue.enqueue_transition_effects(
        &TrackId::new("btc-core"),
        &[TrackEffect::SubmitOrder {
            request: test_order_request(),
            desired_exposure: Exposure(4.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            recovery_token: SubmitRecoveryToken::empty(),
        }],
        Utc::now(),
    );

    let execution = Arc::new(RecordingExecutionPort::default());
    let account = Arc::new(NoopAccountPort::default());
    let worker = EffectWorker::new(
        effect_worker_context,
        execution.clone(),
        account,
        Duration::from_millis(1),
    );

    worker.run_once().await.unwrap();

    assert_eq!(execution.submit_order_call_count(), 1);
}
```

- [ ] **Step 2: 运行 worker 测试确认一个红一个绿目标**

Run:

```bash
cargo test -p poise-server worker_dispatches_current_session_queue_effect worker_does_not_dispatch_persisted_effects_from_previous_session -- --nocapture
```

Expected: `worker_dispatches_current_session_queue_effect` FAIL；旧 DB pending 测试应保持 PASS。

- [ ] **Step 3: 改 `EffectWorkerState`**

在 `server/src/server_context.rs` 中为 `EffectWorkerState` 添加：

```rust
pub session_effect_queue: SessionEffectQueue,
```

在 `build_effect_worker_state(...)` 传入 queue，并在 `server/src/test_support.rs` 增加 `build_effect_worker_context_for_repository_with_queue(...)`。这个 helper 和 Task 1 的 `build_effect_worker_context_for_repository(...)` 使用同一套 server test support，只是把创建出的 queue 一并返回给测试：

```rust
pub(crate) fn build_effect_worker_state(
    reconcile: ReconcileState,
    effect_service: Arc<TrackEffectService>,
    submit_effect_service: Arc<SubmitEffectService>,
    account_margin_guard: Arc<AccountMarginGuardStore>,
    session_effect_queue: SessionEffectQueue,
) -> EffectWorkerState
```

- [ ] **Step 4: 改 worker dispatch**

把 `server/src/effect_worker/dispatch.rs::run_once` 改成：

```rust
pub(super) async fn run_once(worker: &EffectWorker) -> Result<()> {
    loop {
        if *worker.shutdown_rx.borrow() {
            break;
        }

        let Some(effect) = worker.state.session_effect_queue.claim_next() else {
            break;
        };
        let effect_id = effect.effect_id.clone();
        let result = match worker.process_effect(effect.clone()).await {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!("failed to process session effect: {error}");
                SessionDispatchResult::Outcome(SessionEffectOutcome::Blocked {
                    reason: error.to_string(),
                })
            }
        };
        match result {
            SessionDispatchResult::Outcome(outcome) => {
                let action = worker
                    .state
                    .session_effect_queue
                    .record_outcome(&effect_id, outcome);
                worker.handle_session_queue_action(effect.track_id.as_str(), action).await?;
            }
            SessionDispatchResult::Cancel(resolution) => {
                let action = worker
                    .state
                    .session_effect_queue
                    .record_cancel_resolution(&effect_id, resolution);
                worker.handle_cancel_queue_action(effect.track_id.as_str(), action).await?;
            }
        }
    }

    Ok(())
}
```

`handle_session_queue_action(...)` 只处理 queue 返回的领域动作：记录 retired effects 的 best-effort journal outcome，并按需触发 fresh reconcile。它不能重新查询 journal pending 状态。

把 `process_effect` 的返回类型改成：

```rust
pub(super) enum SessionDispatchResult {
    Outcome(SessionEffectOutcome),
    Cancel(CancelReceiptResolution),
}

pub(super) async fn process_effect(
    worker: &EffectWorker,
    effect: SessionTrackEffect,
) -> Result<SessionDispatchResult>
```

- [ ] **Step 5: 改 execute 返回 outcome**

`execute_submit`：

- market freshness gate 返回 `Ok(SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred { until: DeferredUntil::FreshMarket }))`。
- exchange state / reconcile-first gate 返回 `Ok(SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred { until: DeferredUntil::ExchangeState }))`。
- worker 记录 `Deferred` 后不能立刻反复 claim 同一个 effect；对应 track 需要等匹配类型的 fresh input 后调用 `wake_track_for(track_id, signal)`。
- `submit_order(...)` 返回成功 receipt 后，先调用 `session_effect_queue.record_submit_exchange_accepted(&effect.effect_id)`，再调用 application writeback。这样在 exchange accepted 与本地 writeback 之间，exchange sync 能通过 `active_submit_hints_for_track(...)` 识别这个 submit。若该方法返回 `false`，说明 queue 状态与 worker 执行流不一致，应记录诊断日志并返回 blocked/错误路径，不能继续把状态写成 finished。
- submit receipt 写回成功返回 `Ok(SessionDispatchResult::Outcome(SessionEffectOutcome::Finished))`。
- submit failure 且 failure 已写回返回 `Ok(SessionDispatchResult::Outcome(SessionEffectOutcome::Blocked { reason }))`。
- writeback outcome unknown 触发 unknown-outcome recovery 后返回 `Deferred { until: DeferredUntil::ExchangeState }`，保留 `SubmittedAwaitingWriteback` active submit hint，直到完整 exchange sync 解析并退休该 submit effect。

`execute_cancellation`：

- market freshness gate 返回 `Ok(SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred { until: DeferredUntil::FreshMarket }))`。
- exchange state / reconcile-first gate 返回 `Ok(SessionDispatchResult::Outcome(SessionEffectOutcome::Deferred { until: DeferredUntil::ExchangeState }))`。
- cancel 交易所调用成功后不要直接返回 `Finished`；必须调用 application writeback，让 application 吸收 receipt 后返回 `CancelReceiptResolution`，再返回 `SessionDispatchResult::Cancel(resolution)`。
- `ClosedWithoutFill` 通过 `SessionEffectQueue::record_cancel_resolution(...)` 释放同 batch downstream submit。
- `ClosedWithFill` 通过 `SessionEffectQueue::record_cancel_resolution(...)` 废弃同 batch downstream submit，并触发 immediate reconcile。
- `StillWorking` 通过 queue 返回 `CancelQueueAction::Deferred { until: DeferredUntil::ExchangeState }`，保留 cancel effect，不释放 downstream submit；对应 track 只能由 exchange reconcile / open-order sync 的 `WakeSignal::ExchangeState` 唤醒。
- `Unknown` 通过 queue 返回 `AwaitingCancelFollowUp { reason }`，并触发 bounded open-order sync；sync 完成后由 `resolve_cancel_follow_ups_from_open_order_snapshot(...)` 根据完整 `CompleteOpenOrderSnapshot` 在 queue 内部解析 downstream submit。订单已不在 open orders 时废弃 downstream submit 并触发 reconcile；订单仍在 open orders 时消费本次 follow-up，把原 cancel effect 转回 queued，并暂停到下一次 `WakeSignal::ExchangeState` 后重试。

- [ ] **Step 6: 明确并测试 wake owner**

`wake_track_for(track_id, signal)` 的 owner 是产生 fresh runtime 输入的应用层路径，不是 effect worker。实现时只在这些路径调用：

- `observe_market(...)` 成功写入新的 market observation 后，调用 `session_effect_queue.wake_track_for(&track_id, WakeSignal::FreshMarket)`。
- exchange reconcile / open-order sync 成功吸收交易所 position/open orders 后，调用 `session_effect_queue.wake_track_for(&track_id, WakeSignal::ExchangeState)`。

增加验收测试：

```rust
#[tokio::test]
async fn deferred_effect_wakes_after_fresh_market_observation() {
    // seed queue with an effect, run worker until market freshness gate returns Deferred { FreshMarket }
    // assert queue.claim_next() is None for the same track
    // call observe_market(...) with a fresh ExecutionQuote
    // assert queue.claim_next() returns the deferred effect again
}

#[tokio::test]
async fn deferred_effect_wakes_after_exchange_reconcile() {
    // seed queue with an effect, defer it through ReconcileFirst / ExchangeState
    // complete exchange sync/reconcile for that track
    // assert queue.claim_next() returns the deferred effect again
}

#[tokio::test]
async fn deferred_effect_ignores_wrong_wake_signal() {
    // seed queue with an effect deferred until ExchangeState
    // call observe_market(...) and assert the track remains paused
    // call exchange reconcile and assert the effect becomes claimable
}
```

不要让 worker 在 `Deferred` 后通过 sleep / next loop 自行重试；那会回到“同一个 effect 反复占住队列”的旧模型。

- [ ] **Step 7: 跑 worker 测试确认通过**

Run:

```bash
cargo test -p poise-server worker_dispatches_current_session_queue_effect worker_does_not_dispatch_persisted_effects_from_previous_session deferred_effect_wakes_after_fresh_market_observation deferred_effect_wakes_after_exchange_reconcile deferred_effect_ignores_wrong_wake_signal -- --nocapture
```

Expected: PASS。

- [ ] **Step 8: 提交第一个可运行垂直切片**

```bash
git add application/src/session_effect_queue.rs application/src/lib.rs application/src/mutation_executor.rs application/src/track_effect_store.rs application/src/track_mutation_store.rs application/src/track_persistence.rs application/src/runtime_lifecycle_service.rs application/src/track_observation_service.rs application/src/track_command_service.rs storage/src/sqlite.rs server/src/effect_worker server/src/server_context.rs server/src/assembly.rs server/src/runtime/reconcile.rs server/src/test_support.rs
git commit -m "feat: run effects from session queue"
```

## Task 2: exchange sync 与 cancel resolution 改用 session queue

**Files:**

- Modify: `application/src/session_effect_queue.rs`
- Modify: `application/src/mutation_executor.rs`
- Test: `application/src/mutation_executor.rs`

执行记录：

- 2026-04-25：完成 exchange sync active submit hint 切换与 cancel receipt 分类，commit `45cd9aa`。
- 验收：`cargo test -p poise-application mutation_executor::tests:: -- --nocapture`
- 验收：`cargo test -p poise-server effect_worker:: -- --nocapture`

- [ ] **Step 1: 扩展 queue 测试**

在 `application/src/session_effect_queue.rs` tests 中新增：

```rust
#[test]
fn queue_returns_active_submit_hints_for_current_session_only() {
    let queue = SessionEffectQueue::default();
    let enqueued = queue.enqueue_transition_effects(
        &TrackId::new("btc-core"),
        &[submit_effect("client-1")],
        Utc::now(),
    );

    assert!(
        queue
            .active_submit_hints_for_track(&TrackId::new("btc-core"))
            .is_empty(),
        "queued submit that was never claimed is not an exchange fact"
    );

    assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0].effect_id);
    let hints = queue.active_submit_hints_for_track(&TrackId::new("btc-core"));

    assert_eq!(hints.len(), 1);
    assert_eq!(hints[0].request.client_order_id, "client-1");
}

#[test]
fn active_submit_hints_exclude_downstream_submit_after_cancel() {
    let queue = SessionEffectQueue::default();
    let enqueued = queue.enqueue_transition_effects(
        &TrackId::new("btc-core"),
        &[cancel_effect(), submit_effect("client-1")],
        Utc::now(),
    );

    assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0].effect_id);

    assert!(
        queue
            .active_submit_hints_for_track(&TrackId::new("btc-core"))
            .is_empty(),
        "future downstream submit must not be used as exchange sync hint"
    );
}

#[test]
fn cancel_without_fill_unblocks_downstream_submit_effects() {
    let queue = SessionEffectQueue::default();
    let enqueued = queue.enqueue_transition_effects(
        &TrackId::new("btc-core"),
        &[cancel_effect(), submit_effect("client-1"), submit_effect("client-2")],
        Utc::now(),
    );

    assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0].effect_id);

    let action = queue.record_cancel_resolution(
        &enqueued[0].effect_id,
        CancelReceiptResolution::ClosedWithoutFill,
    );

    assert_eq!(action, CancelQueueAction::UnblockedDownstream);
    assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[1].effect_id);
}
```

在测试里定义 `submit_effect(...)` 和 `cancel_effect(...)` helper，使用真实 `TrackEffect::SubmitOrder` 和 `TrackEffect::CancelOrder`。

- [ ] **Step 2: 运行 queue 测试确认失败**

Run:

```bash
cargo test -p poise-application session_effect_queue -- --nocapture
```

Expected: FAIL，因为 active submit hint 查询和 cancel resolution 尚未接入。

- [ ] **Step 3: 实现 queue 查询**

在 `SessionEffectQueue` 添加：

```rust
pub fn active_submit_hints_for_track(&self, track_id: &TrackId) -> Vec<PendingSubmitHint> {
    let inner = self.inner.lock().unwrap();
    let Some(track_queue) = inner.tracks.get(track_id) else {
        return Vec::new();
    };
    track_queue
        .batches
        .iter()
        .flat_map(|batch| batch.effects.iter())
        .filter(|item| {
            matches!(
                item.dispatch_state,
                QueuedEffectState::InFlight | QueuedEffectState::SubmittedAwaitingWriteback
            )
        })
        .filter_map(|item| match &item.effect.effect {
            TrackEffect::SubmitOrder {
                request,
                desired_exposure,
                submit_purpose,
                recovery_token,
            } => Some(PendingSubmitHint {
                request: request.clone(),
                desired_exposure: desired_exposure.clone(),
                submit_purpose: *submit_purpose,
                recovery_token: recovery_token.clone(),
            }),
            _ => None,
        })
        .collect()
}
```

不要添加 `pending_submit_effects_after(...)` 这类公共查询。`batch_id + sequence` 是 queue 内部用来解释 batch 顺序的知识，调用方不能拿它手动查询 downstream submit。也不要把尚未 claim 的 queued submit 返回给 exchange sync；它们只是当前 session 的未来计划，不是交易所事实。

- [ ] **Step 4: 改 MutationExecutor**

在 `sync_exchange_state_inner` 中删除：

```rust
let active_submit_hints = self
    .effect_store
    .list_all_pending_submit_effects()
```

改为：

```rust
let active_submit_hints = self
    .session_effect_queue
    .active_submit_hints_for_track(&TrackId::new(id));
```

在 `record_cancel_order_success` 中删除手工查询 downstream submit 的逻辑。这个方法只做三件事：

1. 把 cancel receipt 吸收到 manager。
2. 根据 manager 是否推进 fill progress 和 receipt status 返回 `CancelReceiptResolution`。
3. 记录 journal outcome 的 best-effort 请求，但不负责释放或废弃 downstream submit。

分类规则：

```rust
if receipt.filled_qty > 0.0 || cancel_receipt_absorbed_exposure(...) {
    CancelReceiptResolution::ClosedWithFill {
        filled_qty: receipt.filled_qty,
    }
} else if receipt.status.clears_working_order() {
    CancelReceiptResolution::ClosedWithoutFill
} else if receipt.status.keeps_working_order() {
    CancelReceiptResolution::StillWorking
} else {
    CancelReceiptResolution::Unknown {
        reason: format!("unexpected cancel receipt status: {:?}", receipt.status),
    }
}
```

在 bounded open-order sync 完成后的 follow-up 路径中，不再查询 downstream submit 列表，也不构造 `FollowUpRetirementRequest`。该路径只把完整 `CompleteOpenOrderSnapshot` 提交给 `resolve_cancel_follow_ups_from_open_order_snapshot(...)`；queue 内部根据之前保存的 follow-up 指针判断 unknown cancel 对应订单是仍在 open orders 还是已经关闭。

`record_cancel_order_success` 不再直接调用 `pending_submit_effects_after(...)`；同 batch downstream 的释放/废弃由 worker 调用 `SessionEffectQueue::record_cancel_resolution(...)` 完成。

在 `application/src/mutation_executor.rs` 增加两个验收测试：

```rust
#[tokio::test]
async fn cancel_receipt_without_fill_resolves_closed_without_fill() {
    // seed cancel-pending binding, return Canceled + filled_qty 0
    // assert record_cancel_order_success returns CancelReceiptResolution::ClosedWithoutFill
}

#[tokio::test]
async fn cancel_receipt_with_fill_resolves_closed_with_fill() {
    // seed cancel-pending binding, return Canceled/Filled + filled_qty > 0
    // assert record_cancel_order_success returns CancelReceiptResolution::ClosedWithFill
    // assert downstream submit effects are not restored by MutationExecutor
}
```

- [ ] **Step 5: 运行应用测试**

Run:

```bash
cargo test -p poise-application mutation_executor::tests:: session_effect_queue -- --nocapture
```

Expected: PASS。

- [ ] **Step 6: 提交**

```bash
git add application/src/session_effect_queue.rs application/src/mutation_executor.rs
git commit -m "refactor: source pending session effects from queue"
```

## Task 3: cancel follow-up 改为 queue-owned session 状态

**Files:**

- Modify: `application/src/session_effect_queue.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/track_effect_store.rs`
- Modify: `storage/src/sqlite.rs`
- Test: `application/src/session_effect_queue.rs`
- Test: `application/src/mutation_executor.rs`

- [x] **Step 1: 写 queue 测试：cancel follow-up 由完整 exchange snapshot 解释**

在 `application/src/session_effect_queue.rs` tests 中新增：

```rust
#[test]
fn cancel_follow_up_is_resolved_from_complete_open_order_snapshot() {
    let queue = SessionEffectQueue::default();
    let enqueued = queue.enqueue_transition_effects(
        &TrackId::new("btc-core"),
        &[cancel_effect(), submit_effect("submit-1"), submit_effect("submit-2")],
        Utc::now(),
    );

    assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0].effect_id);
    let action = queue.record_cancel_resolution(
        &enqueued[0].effect_id,
        CancelReceiptResolution::Unknown {
            order_id: "closed-order".into(),
            reason: "exchange returned unknown order".into(),
        },
    );
    assert!(matches!(action, CancelQueueAction::AwaitingCancelFollowUp { .. }));

    let actions = queue.resolve_cancel_follow_ups_from_open_order_snapshot(
        &TrackId::new("btc-core"),
        &complete_open_orders(&[]),
    );

    assert_eq!(
        actions,
        vec![FollowUpQueueAction::SupersededDownstream {
            effect_ids: vec![enqueued[1].effect_id.clone(), enqueued[2].effect_id.clone()],
            requires_reconcile: true,
        }]
    );
}
```

在 `application/src/mutation_executor.rs` tests 中新增：

```rust
#[tokio::test]
async fn fresh_session_clears_session_queue_without_store_cleanup() {
    let queue = SessionEffectQueue::default();
    queue.enqueue_transition_effects(&TrackId::new("btc-core"), &[submit_effect("submit-1")], Utc::now());
    let (services, _) = track_write_services_with_queue(seeded_manager(), queue.clone());

    services
        .runtime_lifecycle
        .prepare_fresh_session_for_activation("btc-core")
        .await
        .unwrap();

    assert!(queue.claim_next().is_none());
}
```

这个测试不 seed repository，也不检查 deleted count。它验证的是新共识：fresh session 只清当前 session queue，不读取或清理 durable follow-up retirement。

- [x] **Step 2: 运行测试确认失败**

Run:

```bash
cargo test -p poise-application cancel_follow_up_is_resolved_from_complete_open_order_snapshot fresh_session_clears_session_queue_without_store_cleanup -- --nocapture
```

Expected: FAIL，因为 queue 还没有实现 cancel follow-up 领域动作。

- [x] **Step 3: 将 cancel follow-up 放入 session queue**

在 `SessionEffectQueue` 增加：

```rust
pub fn resolve_cancel_follow_ups_from_open_order_snapshot(
    &self,
    track_id: &TrackId,
    open_orders: &CompleteOpenOrderSnapshot,
) -> Vec<FollowUpQueueAction>;
```

`resolve_cancel_follow_ups_from_open_order_snapshot(...)` 是单一领域动作。bounded open-order sync 完成后，调用方只提交完整 `CompleteOpenOrderSnapshot`；queue 内部根据之前保存的 follow-up 指针解释结果。订单已不在 open orders 时，返回 `FollowUpQueueAction::SupersededDownstream` 并把这些 submit 从 queue 中移除。订单仍在 open orders 时，queue 消费 follow-up 指针，把原 cancel effect 转回 queued，暂停到下一次 `WakeSignal::ExchangeState`，并且不释放 downstream submit。调用方不保存 token 生命周期，也不直接读取或构造 `batch_id + sequence`。

在 `MutationExecutor` 中处理 unknown cancel 时：

```rust
let cancel_action = self
    .session_effect_queue
    .record_cancel_resolution(effect_id, CancelReceiptResolution::Unknown { order_id, reason });
if let CancelQueueAction::AwaitingCancelFollowUp { .. } = cancel_action {
    self.request_bounded_open_order_sync(id).await?;
}
Ok(())
```

bounded open-order sync 完成后，只把完整 `CompleteOpenOrderSnapshot` 交给 queue：

```rust
let actions = self
    .session_effect_queue
    .resolve_cancel_follow_ups_from_open_order_snapshot(&TrackId::new(id), &open_orders);
for action in actions {
    self.handle_follow_up_queue_action(id, action).await?;
}
```

删除 `retry_pending_follow_up_retirements_best_effort`。cancel follow-up 不再有 pending retry 概念；如果本次 request 没有匹配到当前 session queue 中的 downstream submit，queue 返回 `NothingToRetire` 或 `Blocked`，后续由新的 reconcile / exchange sync 重新规划。

如果当前代码没有独立 `handle_follow_up_queue_action(...)`，本 task 新增一个私有 helper。它只根据 `FollowUpQueueAction` 做两类事：

- `SupersededDownstream`：best-effort 写 journal outcomes，并触发 fresh reconcile。
- `StillWorking`：记录诊断即可；queue 已经把原 cancel effect 放回 queued 并暂停到下一次 exchange-state wake。
- `NothingToRetire` / `Blocked`：记录日志或指标，不释放任何 submit。

- [x] **Step 4: 删除 startup durable retirement 清理**

从 `prepare_fresh_session_for_activation` 删除：

```rust
let follow_up_retirements = self.effect_store.list_follow_up_retirement_requests(...).await?;
for request in &follow_up_retirements {
    self.effect_store.delete_follow_up_retirement_request(...).await?;
}
```

保留：

```rust
self.session_effect_queue.clear_track(&TrackId::new(id));
```

- [x] **Step 5: 删除 durable follow-up retirement store 接口**

从旧 effect store/journal 边界和 SQLite 删除：

```rust
async fn save_follow_up_retirement_request(...)
async fn list_follow_up_retirement_requests(...)
async fn delete_follow_up_retirement_request(...)
```

这一步应让“重启后恢复旧 follow-up retirement”在类型层面无法发生。

- [x] **Step 6: 运行测试**

Run:

```bash
cargo test -p poise-application mutation_executor::tests:: runtime_lifecycle_service::tests:: -- --nocapture
```

Expected: PASS。

- [x] **Step 7: 提交**

```bash
git add application/src/session_effect_queue.rs application/src/mutation_executor.rs application/src/track_effect_store.rs storage/src/sqlite.rs
git commit -m "refactor: make follow-up retirement session scoped"
```

执行记录：

- 2026-04-25：完成 follow-up retirement session queue 化，commit `d154141`。
- 2026-04-25：收窄 queue 公共入口，batch/effect identity 由 `SessionEffectQueue` 内部生成；follow-up retirement 改为完整 open-orders snapshot 驱动，不再公开 token 协议，commit `b6a9c3e`。
- 2026-04-26：同步 spec/plan 中 queue-owned identity 的文档示例，避免把 `SessionTrackEffect` 的 batch/sequence 误读为公开协议，commit `c7df57c`。
- 2026-04-26：补齐 unknown cancel follow-up 的 still-open 分支，完整 open-orders 证明原订单仍 open 时重试原 cancel 且不释放 downstream submit，commit `25b6620`。
- 2026-04-26：将 cancel follow-up public API 改为接收完整 `CompleteOpenOrderSnapshot`，queue 内部解释 closed/still-open 结果，commit `72053ae`。
- 验收：`cargo test -p poise-application session_effect_queue -- --nocapture`
- 验收：`cargo test -p poise-application mutation_executor::tests::record_cancel_order_success -- --nocapture`
- 验收：`cargo test -p poise-application cancel_follow_up_is_resolved_from_complete_open_order_snapshot -- --nocapture`
- 验收：`cargo test -p poise-application exchange_sync_records_cancel_follow_up_outcomes -- --nocapture`
- 验收：`cargo test -p poise-server effect_worker -- --nocapture`
- 验收：`cargo test -p poise-application runtime_lifecycle_service::tests::prepare_fresh_session_for_activation_clears_old_pending_work_and_executor_state -- --nocapture`
- 验收：`cargo test -p poise-storage schema -- --nocapture`
- 验收：`cargo test -p poise-server effect_worker:: -- --nocapture`
- 验收：`cargo test -p poise-server runtime::submit_preflight::tests::reconcile_ignores_persisted_effects_from_previous_session -- --nocapture`
- 验收：`cargo test -p poise-server runtime::startup_bootstrap::tests::complete_startup_cancels_inherited_orders_and_rebuilds_fresh_executor_state -- --nocapture`

## Task 4: startup 删除旧 effect 清理依赖

**Files:**

- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/runtime_lifecycle_service.rs`
- Modify: `application/src/track_effect_store.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `server/src/runtime/startup_bootstrap.rs`
- Test: `application/src/runtime_lifecycle_service.rs`

- [x] **Step 1: 写测试：fresh activation 不 supersede 旧 DB effects**

在 `application/src/runtime_lifecycle_service.rs` tests 中替换旧测试 `prepare_fresh_session_for_activation_clears_old_pending_work_and_executor_state`，新测试为：

```rust
#[tokio::test]
async fn prepare_fresh_session_for_activation_does_not_mutate_old_persisted_effects() {
    let repository = Arc::new(MemoryRepository::default());
    let (services, _) = track_write_services(seeded_manager(), repository.clone());
    repository.seed_pending_mixed_effect_batch("btc-core", "btc-core:batch-1");

    services
        .runtime_lifecycle
        .prepare_fresh_session_for_activation("btc-core")
        .await
        .unwrap();

    let effects = repository.pending_effects();
    let statuses = effects
        .iter()
        .map(|effect| effect.status)
        .collect::<Vec<_>>();
    assert_eq!(
        statuses,
        vec![EffectStatus::Pending, EffectStatus::Pending],
        "old persisted effects are journal history, not startup work"
    );
}
```

- [x] **Step 2: 运行测试确认失败**

Run:

```bash
cargo test -p poise-application prepare_fresh_session_for_activation_does_not_mutate_old_persisted_effects -- --nocapture
```

Expected: FAIL，因为当前 activation 会 supersede 旧 effect。

- [x] **Step 3: 简化 activation**

把 `MutationExecutor::prepare_fresh_session_for_activation` 改成：

```rust
pub(crate) async fn prepare_fresh_session_for_activation(&self, id: &str) -> Result<()> {
    let _mutation_guard = self.lock_track_mutation(id).await;
    let track_id = TrackId::new(id);
    self.session_effect_queue.clear_track(&track_id);

    let mut manager = self.manager.write().await;
    manager
        .reset_executor_for_activation(&track_id)
        .map_err(TrackMutationError::Mutation)?;
    Ok(())
}
```

- [x] **Step 4: 删除不再使用的 store 方法**

从旧 effect store/journal 边界删除：

```rust
async fn list_session_reset_effects_for_track(...)
async fn list_follow_up_retirement_requests(...)
async fn delete_follow_up_retirement_request(...)
```

如果 Task 3 已经删除 durable follow-up retirement，继续删除：

```rust
async fn save_follow_up_retirement_request(...)
```

同步删除 SQLite 和 test repository 实现。

- [x] **Step 5: 运行测试**

Run:

```bash
cargo test -p poise-application runtime_lifecycle_service::tests:: -- --nocapture
cargo check -p poise-application -p poise-storage -p poise-server
```

Expected: PASS。

- [x] **Step 6: 提交**

```bash
git add application/src/mutation_executor.rs application/src/runtime_lifecycle_service.rs application/src/track_effect_store.rs storage/src/sqlite.rs server/src/runtime/startup_bootstrap.rs
git commit -m "refactor: remove startup persisted effect cleanup"
```

执行记录：

- 2026-04-25：删除 startup 对旧 persisted effect cleanup 的依赖，commit `11d8031`。
- 验收：`cargo test -p poise-application prepare_fresh_session_for_activation_does_not_mutate_old_persisted_effects -- --nocapture` 先失败，确认旧实现会 supersede 旧 DB effects。
- 验收：`cargo test -p poise-application runtime_lifecycle_service::tests:: -- --nocapture`
- 验收：`cargo test -p poise-server runtime::startup_bootstrap::tests::complete_startup_cancels_inherited_orders_and_rebuilds_fresh_executor_state -- --nocapture`
- 验收：`cargo test -p poise-storage list_pending_submit_effects_for_track_batch_returns_same_batch_submit_without_ready_filter -- --nocapture`
- 验收：`cargo check -p poise-application -p poise-storage -p poise-server`

## Task 5: 删除旧 effect store 的 dispatch/pending 残留

**Files:**

- Modify: `application/src/track_effect_store.rs`
- Modify: `application/src/track_query_store.rs`
- Modify: `application/src/read_model.rs`
- Modify: `application/src/query_service.rs`
- Modify: `application/src/debug_query_service.rs`
- Modify: `storage/src/sqlite.rs`
- Modify: `storage/src/schema.rs`
- Modify: `server/src/projector.rs`
- Test: `storage/src/sqlite.rs`
- Test: `application/src/query_service.rs`

- [x] **Step 1: 确认 journal 接口没有运行队列语义**

`application/src/track_effect_store.rs` 中最终只保留 Task 1 引入的 journal 接口：

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

确认 `TrackMutationStore` 中没有 effect journal 写入口：

```rust
async fn update_effect_status(...);   // remove
async fn update_effect_statuses(...); // remove
```

effect outcome 写入只通过 `TrackEffectJournal::record_effect_outcomes(...)` 发生，并且调用方按诊断写入处理：失败记录日志或指标，不改变 `SessionEffectQueue` 的运行判定。

同步更新 Task 1 的 `seed_persisted_pending_submit_effect(...)` test helper：最终代码里它应通过 `TrackEffectJournal::append_entries(...)` seed 旧 journal row，不再通过 `TrackMutationStore::commit_track_transition(...)` 写 effect。

- [x] **Step 2: 删除 dispatch/pending 查询实现**

从 SQLite 删除或停止公开：

```rust
list_dispatchable_effects_blocking
list_pending_submit_effects_for_track_blocking
list_pending_submit_effects_for_track_batch_blocking
list_all_pending_effects_for_track_blocking
```

保留：

```rust
list_recent_track_effects_blocking
```

- [x] **Step 3: 调整 schema 索引**

在 `storage/src/schema.rs` 删除 dispatch 专用索引：

```sql
DROP INDEX IF EXISTS idx_track_effects_pending;
DROP INDEX IF EXISTS idx_track_effects_batch_sequence;
```

保留 recent 查询索引：

```sql
CREATE INDEX IF NOT EXISTS idx_track_effects_recent
ON track_effects(track_id, updated_at DESC, created_at DESC, batch_id DESC, sequence DESC, effect_id DESC);
```

- [x] **Step 4: 调整测试**

删除这些 storage 测试或改成 queue 测试：

```text
list_pending_effects_only_returns_batch_head_until_prior_effect_succeeds
list_pending_effects_advances_after_prior_effect_is_superseded
list_pending_effects_keeps_follow_up_blocked_after_prior_failure
list_pending_submit_effects_for_track_returns_only_dispatchable_submit_effects
list_pending_submit_effects_for_track_batch_returns_same_batch_submit_without_ready_filter
list_session_reset_effects_for_track_returns_pending_and_executing_effects
```

保留并强化：

```text
list_recent_track_effects_filters_by_track_id_and_limit
list_recent_track_effects_orders_results_by_updated_at
record_effect_outcomes_updates_recent_journal_without_dispatch_semantics
```

- [x] **Step 5: 运行测试**

Run:

```bash
cargo test -p poise-storage recent_track_effects effect_journal -- --nocapture
cargo check -p poise-application -p poise-storage -p poise-server
```

Expected: PASS。

- [x] **Step 6: 提交**

```bash
git add application/src/track_effect_store.rs application/src/track_query_store.rs application/src/read_model.rs application/src/query_service.rs application/src/debug_query_service.rs storage/src/sqlite.rs storage/src/schema.rs server/src/projector.rs
git commit -m "refactor: make track effects a journal"
```

执行记录：

- 2026-04-25：删除旧 effect store 的 dispatch/pending/status 运行边界，把 effect 持久化降级为诊断 journal，commit `6f196de`。
- 2026-04-25：补齐 current-session queue 与 journal 边界，effect-only transition 不再写业务真值，follow-up retirement / retired batch 只通过 queue 动作退休 session effect 并 best-effort 更新诊断 journal，commit `3929a10`。
- 2026-04-25：补齐 submit writeback unknown 的 active hint 保留与 exchange sync 退休路径，移除 cancel writeback API 的 batch/sequence 泄漏，并让 `EffectJournalEntry` 不再依赖 session queue 运行类型，commit `0e9177a`。
- 验收：`cargo test -p poise-application mutation_executor::tests:: -- --nocapture`
- 验收：`cargo test -p poise-application session_effect_queue::tests:: -- --nocapture`
- 验收：`cargo test -p poise-storage schema::tests::initialize_creates_tables -- --nocapture`
- 验收：`cargo test -p poise-storage list_recent_track_effects -- --nocapture`
- 验收：`cargo test -p poise-storage save_transition_persists_events_and_effect_journal_entries_atomically -- --nocapture`
- 验收：`cargo test -p poise-server runtime::submit_preflight::tests::reconcile_ignores_persisted_effects_from_previous_session -- --nocapture`
- 验收：`cargo test -p poise-server effect_worker:: -- --nocapture`
- 验收：`cargo check -p poise-application -p poise-storage -p poise-server`

## Task 6: 更新 TUI/HTTP 展示语义

**Files:**

- Modify: `application/src/read_model.rs`
- Modify: `application/src/query_service.rs`
- Modify: `server/src/projector.rs`
- Modify: `tui/src/views/instance.rs`
- Test: `server/src/projector.rs`

- [ ] **Step 1: 写 projector 测试**

在 `server/src/projector.rs` 添加测试：

```rust
#[test]
fn projector_labels_effects_as_history_not_current_execution_queue() {
    let mut source = source_with_submitting_effect();
    source.recent_effects[0].status = EffectStatus::Pending;

    let detail = TrackProjector::new().project_detail(&source);

    assert!(
        detail
            .execution
            .lines
            .iter()
            .any(|line| line.contains("effect history")),
        "pending journal effect should be rendered as history, not as current queue work"
    );
}

#[test]
fn projector_renders_current_session_queue_from_snapshot() {
    let mut source = source_with_empty_effect_history();
    source.current_session_queue = Some(queue_snapshot_with_queued_submit());

    let detail = TrackProjector::new().project_detail(&source);

    assert!(
        detail
            .execution
            .lines
            .iter()
            .any(|line| line.contains("current queue")),
        "current queue should come from SessionEffectQueueSnapshot"
    );
}
```

- [ ] **Step 2: 运行测试确认失败**

Run:

```bash
cargo test -p poise-server projector_labels_effects_as_history_not_current_execution_queue projector_renders_current_session_queue_from_snapshot -- --nocapture
```

Expected: FAIL，当前文案仍像当前执行状态。

- [ ] **Step 3: 改 read model / projector**

将 read model 中 effect 区命名为 history，并新增当前 session queue 的展示 DTO：

```rust
pub recent_effect_history: Vec<PersistedTrackEffect>,
pub current_session_queue: Option<SessionEffectQueueSnapshot>,
```

`current_session_queue` 来自 `SessionEffectQueue::snapshot_for_track(...)`。query/read model 层只能消费 `SessionEffectQueueSnapshot`，不能拿 `SessionTrackEffect`、`batch_id` 或 `sequence`。如果某个查询路径拿不到当前进程内 queue，就显示 `None`，不要从 journal 的 pending 状态反推当前 queue。

Projector 文案改为：

```text
effect history: pending submit ...
current queue: queued cancel ...
```

当前 session queue 单独由 runtime live source 提供，不从 journal 推断。

- [ ] **Step 4: 运行 projector 测试**

Run:

```bash
cargo test -p poise-server projector::tests:: -- --nocapture
```

Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add application/src/read_model.rs server/src/projector.rs tui/src/views/instance.rs
git commit -m "ui: label persisted effects as history"
```

## Task 7: 文档和最终验收

**Files:**

- Modify: `docs/superpowers/specs/2026-04-25-session-effect-queue-and-journal-design.md`
- Modify: `docs/superpowers/plans/2026-04-25-session-effect-queue-and-journal.md`
- Modify: `docs/superpowers/specs/2026-04-23-track-session-runtime-fresh-start-design.md`
- Modify: `docs/superpowers/plans/2026-04-23-track-session-runtime-fresh-start.md`

- [ ] **Step 1: 回写旧 fresh-session 文档**

在 `2026-04-23-track-session-runtime-fresh-start-design.md` 的 effect/follow-up retirement 相关段落中明确：

```markdown
旧 session effect 不再作为 startup 清理对象。新的 effect 边界见
[2026-04-25-session-effect-queue-and-journal-design.md](2026-04-25-session-effect-queue-and-journal-design.md)。
```

- [ ] **Step 2: 删除旧 plan 中“supersede pending effect”心智模型**

在 `2026-04-23-track-session-runtime-fresh-start.md` 中删除或替换：

```text
fresh-session 清理 Pending + Executing
新增按 track 查询并 supersede 可作废旧会话 effect
```

替换为：

```text
fresh-session 清空当前进程 SessionEffectQueue；旧 persisted effect 只作为 journal 历史，不参与 startup。
```

- [ ] **Step 3: 运行最小验收**

Run:

```bash
cargo test -p poise-application session_effect_queue mutation_executor::tests:: runtime_lifecycle_service::tests:: -- --nocapture
cargo test -p poise-server effect_worker projector::tests:: -- --nocapture
cargo test -p poise-storage recent_track_effects effect_journal -- --nocapture
cargo check -p poise-application -p poise-storage -p poise-server
```

Expected: PASS。

- [ ] **Step 4: 确认没有旧 dispatch 查询残留**

Run:

```bash
rg "list_dispatchable_effects|list_session_reset_effects_for_track|list_pending_submit_effects_for_track|list_pending_submit_effects_for_track_batch|follow_up_retirements" application server storage
```

Expected: 只允许出现历史迁移说明或删除迁移测试；生产运行路径不应再引用这些名称。

- [ ] **Step 5: 提交**

```bash
git add docs/superpowers/specs/2026-04-25-session-effect-queue-and-journal-design.md docs/superpowers/plans/2026-04-25-session-effect-queue-and-journal.md docs/superpowers/specs/2026-04-23-track-session-runtime-fresh-start-design.md docs/superpowers/plans/2026-04-23-track-session-runtime-fresh-start.md
git commit -m "docs: define session effect queue boundary"
```

## 自检

- Spec 覆盖：
  - 重启不 replay 旧 effect：Task 1、Task 4、Task 7。
  - 当前 session effect 正常执行：Task 1。
  - cancel 带成交不会释放旧 downstream submit：Task 1、Task 2。
  - active submit hints 不从 DB 来：Task 2。
  - follow-up retirement 不跨重启：Task 3、Task 4。
  - effect store 降级 journal：Task 1、Task 5、Task 6。
  - journal outcome 不挂在 `TrackMutationStore`：Task 1、Task 5。
  - 文档旧模型清理：Task 7。
- 计划不要求保留旧持久 outbox。
- 每个任务都有失败测试、实现、验证、提交步骤。
- 最终架构仍保留账务、交易历史和诊断历史持久化。
