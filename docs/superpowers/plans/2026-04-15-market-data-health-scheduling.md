# Market Data Health Scheduling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `market_data_stale` 的后台调度从 `recovery` 任务中拆出，移除 `50ms` 全量 sweep，改成独立的 market data health scheduler，同时保留“无后续 tick 也能按时标 stale”的行为。

**Architecture:** `engine/application` 继续拥有 stale 规则与逻辑 deadline 计算，只新增 `market_data_health_deadline(...)` 这个窄查询；`server` 继续直接持有 `ClockPort`，并新增独立 `market_data_health_task` 和私有 `MarketDataHealthState`。`MarketDataHealthState` 只维护 dirty tracks 与 notify，deadline map 留在 task 局部状态。market worker 在成功写入 tick 后标记 dirty，health task 仅在存在 deadline 时挂 bounded sleep；没有 deadline 时只被 dirty/shutdown 唤醒，`recovery` 则只保留 anomaly retry / audit。

**Tech Stack:** Rust workspace, Tokio, Cargo tests, chrono, Markdown

---

## Files And Responsibilities

- Create: `server/src/runtime/market_data_health.rs`
  独立拥有 `MarketDataHealthState`、deadline 调度循环、seed/dirty/recheck 逻辑，以及 market data health 任务的私有实现细节。
- Modify: `engine/src/manager.rs`
  新增 engine-owned 的 market data health 查询接口，统一返回 stale 检查 deadline。
- Modify: `application/src/mutation_executor.rs`
  把 manager 的 deadline 查询收成 application 边界方法。
- Modify: `application/src/track_observation_service.rs`
  暴露 `market_data_health_deadline(...)` 给 runtime 使用。
- Modify: `server/src/assembly.rs`
  把 `ClockPort` 注入 runtime 依赖，而不是经由 application 转发。
- Modify: `server/src/runtime/mod.rs`
  注册新模块、新任务、新 handle 字段，并给 runtime 增加 `clock` 和 `market_data_health_max_sleep_interval` 配置。
- Modify: `server/src/runtime/market_data.rs`
  在 `observe_market(...)` 成功后标记对应 `track_id` 的 market data health dirty。
- Modify: `server/src/runtime/reconcile.rs`
  删除 `refresh_market_data_health()` 的 `50ms` 全量 sweep，只保留 recovery anomaly retry / audit。
- Modify: `server/src/runtime/tests/support.rs`
  更新 runtime fixture，使测试可继续注入 `ClockPort` 和 `market_data_health_max_sleep_interval`。
- Modify: `server/src/runtime/tests/startup_sync.rs`
  锁住“无后续 tick 也会按时标 stale”行为，以及 market subscription 空闲时的 deadline seed。
- Modify: `server/src/runtime/tests/reconcile.rs`
  锁住 recovery 任务不再负责 market data health refresh。
- Modify: `docs/superpowers/specs/2026-04-15-market-data-health-scheduling-design.md`
  同步最终确定的 `ClockPort` owner 和 `MarketDataHealthState` 边界。
- Modify: `docs/superpowers/plans/2026-04-15-market-data-health-scheduling.md`
  执行时勾选步骤并记录每个 task 的 commit SHA。

### Task 1: 定义 engine/application 的 market data health deadline 查询边界

**Files:**
- Modify: `engine/src/manager.rs`
- Modify: `application/src/mutation_executor.rs`
- Modify: `application/src/track_observation_service.rs`
- Test: `engine/src/manager.rs`
- Test: `application/src/mutation_executor.rs`

- [x] **Step 1: 先写失败测试，锁住 deadline 查询语义**

在 `engine/src/manager.rs` 增加至少这 3 条测试，在 `application/src/mutation_executor.rs` 增加 1 条 forwarding 测试：

```rust
#[test]
fn market_data_health_deadline_returns_none_without_tick() {}

#[test]
fn market_data_health_deadline_returns_timeout_after_last_tick() {}

#[test]
fn market_data_health_deadline_returns_none_when_track_is_already_stale() {}

#[tokio::test]
async fn mutation_executor_exposes_market_data_health_deadline() {}
```

覆盖点：

- 未收到 tick 时返回 `None`
- fresh tick 后返回 `Some(last_tick_at + tick_timeout_secs)`
- 已经 stale 时返回 `None`
- application 侧只是 forwarding，不重复实现 stale 规则

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-engine market_data_health_deadline_returns_none_without_tick -- --exact`
- `cargo test -p poise-engine market_data_health_deadline_returns_timeout_after_last_tick -- --exact`
- `cargo test -p poise-engine market_data_health_deadline_returns_none_when_track_is_already_stale -- --exact`
- `cargo test -p poise-application mutation_executor::tests::mutation_executor_exposes_market_data_health_deadline -- --exact`

Expected:

- `TrackManager` 还没有 deadline 查询
- `MutationExecutor` / `TrackObservationService` 还没有对应接口

- [x] **Step 3: 在 engine 内实现最小 deadline 查询接口**

在 `engine/src/manager.rs` 增加：

```rust
pub fn market_data_health_deadline(
    &self,
    id: &TrackId,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    let track = self
        .tracks
        .get(id)
        .ok_or_else(|| anyhow::anyhow!("track `{}` not found", id.as_str()))?;

    if track.market_data_stale_since.is_some() {
        return Ok(None);
    }

    let Some(last_tick_at) = track.last_tick_at else {
        return Ok(None);
    };

    let timeout = chrono::Duration::seconds(
        i64::try_from(track.tick_timeout_secs).unwrap_or(30),
    );
    Ok(Some(last_tick_at + timeout))
}
```

要求：

- 不在 server 复制 stale 规则
- 不返回 `Duration`
- 不额外增加基础设施 passthrough 查询

- [x] **Step 4: 在 application 边界暴露 forwarding 方法**

在 `application/src/mutation_executor.rs` 和 `application/src/track_observation_service.rs` 增加：

```rust
pub(crate) async fn market_data_health_deadline(
    &self,
    id: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    let manager = self.manager.read().await;
    manager
        .market_data_health_deadline(&TrackId::new(id))
        .map_err(anyhow::Error::new)
}
```

`TrackObservationService` 只做一层直接转发：

```rust
pub async fn market_data_health_deadline(
    &self,
    id: &str,
) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
    self.executor.market_data_health_deadline(id).await
}
```

- [x] **Step 5: 跑 Task 1 回归**

Run:

- `cargo test -p poise-engine market_data_health_deadline_ -- --nocapture`
- `cargo test -p poise-application mutation_executor::tests::mutation_executor_exposes_market_data_health_deadline -- --exact --nocapture`

Expected:

- engine 层拥有全部 stale deadline 规则
- application 只负责 forwarding

- [x] **Step 6: Commit**

```bash
git add engine/src/manager.rs application/src/mutation_executor.rs application/src/track_observation_service.rs docs/superpowers/plans/2026-04-15-market-data-health-scheduling.md
git commit -m "feat(runtime): expose market data health deadline queries"
```

实现提交：`e262fc2`

### Task 2: 增加独立的 market data health task 和私有 dirty state

**Files:**
- Create: `server/src/runtime/market_data_health.rs`
- Modify: `server/src/runtime/mod.rs`
- Modify: `server/src/runtime/market_data.rs`
- Modify: `server/src/runtime/reconcile.rs`
- Test: `server/src/runtime/market_data_health.rs`
- Test: `server/src/runtime/tests/startup_sync.rs`
- Test: `server/src/runtime/tests/reconcile.rs`

- [x] **Step 1: 先写失败测试，锁住独立调度与 recovery 解耦**

新增或改写至少这些测试：

```rust
#[test]
fn market_data_health_state_coalesces_dirty_track_ids() {}

#[tokio::test]
async fn background_health_check_marks_market_data_stale_without_follow_up_events() {}

#[tokio::test]
async fn recovery_task_does_not_refresh_market_data_health() {}
```

要求：

- 保留现有 `background_health_check_marks_market_data_stale_without_follow_up_events`
- 新增一个只跑 recovery task 的测试，确认把时钟推进到超时后，不会因为 recovery 自己而把 `market_data_stale_since` 写出来

- [x] **Step 2: 运行定向测试，确认当前实现失败**

Run:

- `cargo test -p poise-server runtime::tests::startup_sync::background_health_check_marks_market_data_stale_without_follow_up_events -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::reconcile::recovery_task_does_not_refresh_market_data_health -- --exact --nocapture`

Expected:

- 第一个测试当前仍然通过，作为行为基线
- 第二个新测试失败，因为 recovery 现在还在做 market data health refresh

- [x] **Step 3: 新建私有 `MarketDataHealthState` 与 task 模块**

在 `server/src/runtime/market_data_health.rs` 实现：

```rust
#[derive(Default)]
pub(crate) struct MarketDataHealthState {
    dirty_tracks: std::sync::Mutex<std::collections::HashSet<String>>,
    notify: tokio::sync::Notify,
}

impl MarketDataHealthState {
    pub(crate) fn mark_dirty(&self, track_id: &str) {
        self.dirty_tracks
            .lock()
            .unwrap()
            .insert(track_id.to_string());
        self.notify.notify_one();
    }

    fn take_dirty(&self) -> std::collections::HashSet<String> {
        std::mem::take(&mut *self.dirty_tracks.lock().unwrap())
    }

    async fn wait(&self) {
        self.notify.notified().await;
    }
}
```

同时新增 task 入口：

```rust
pub(super) fn spawn_market_data_health_task(
    runtime: &ServerRuntime,
    shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()>
```

内部维护：

- task 局部 `HashMap<String, chrono::DateTime<Utc>>` 逻辑 deadline
- 共享 `MarketDataHealthState` 中的 dirty tracks
- bounded sleep

- [x] **Step 4: 实现 bounded sleep 调度循环**

主循环按这个顺序工作：

```rust
loop {
    apply_dirty_tracks(...).await;

    let now = runtime.clock.now();
    let due = collect_due_tracks(&deadlines, now);
    if !due.is_empty() {
        refresh_due_tracks(...).await;
        continue;
    }

    if let Some(sleep_for) = next_sleep_interval(&deadlines, now, max_sleep_interval) {
        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => { ... }
            _ = market_data_health_state.wait() => { ... }
            _ = tokio::time::sleep(sleep_for) => {}
        }
    } else {
        tokio::select! {
            biased;
            changed = shutdown_rx.changed() => { ... }
            _ = market_data_health_state.wait() => { ... }
        }
    }
}
```

具体要求：

- `next_sleep_interval(...)` 返回：
  - `None`，当没有 deadline 时
  - `Some(min(deadline - now, max_sleep_interval))`，当有最近 deadline 时
- `refresh_due_tracks(...)` 只对 due tracks 调 `refresh_market_data_health(track_id)`
- refresh 后必须重新查询该 track 的 deadline

- [x] **Step 5: 在 `ServerRuntime` 里注册新任务、`ClockPort` 和配置**

修改 `server/src/runtime/mod.rs`：

```rust
mod market_data_health;

pub struct RuntimeHandles {
    pub market_task: JoinHandle<()>,
    pub market_data_health_task: JoinHandle<()>,
    // ...
}
```

在 `ServerRuntime` 增加：

```rust
clock: Arc<dyn ClockPort>,
market_data_health_state: Arc<MarketDataHealthState>,
market_data_health_max_sleep_interval: Duration,
```

并同步更新：

- `server/src/assembly.rs` 的 runtime 构造
- `server/src/runtime/tests/support.rs` 的测试夹具注入
- `server/src/runtime/market_data.rs` 成功写入 tick 后标记 dirty
- `server/src/runtime/reconcile.rs` 删除旧的 `refresh_market_data_health()` sweep

测试默认值：

- 生产默认：`Duration::from_secs(1)`
- 测试夹具可配置更小间隔，默认继续给 `50ms`

并在 `start()` / `shutdown()` 中加入 spawn / abort / await。

- [x] **Step 6: 跑 Task 2 回归**

Run:

- `cargo test -p poise-server runtime::tests::startup_sync::background_health_check_marks_market_data_stale_without_follow_up_events -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::reconcile::recovery_task_does_not_refresh_market_data_health -- --exact --nocapture`
- `cargo test -p poise-server runtime::market_data_health::tests:: -- --nocapture`

Expected:

- background stale 行为保持
- recovery task 不再负责 market data health
- 新 task 的 dirty / sleep / due 行为有单测覆盖

- [x] **Step 7: Commit**

```bash
git add server/src/runtime/market_data_health.rs server/src/runtime/mod.rs server/src/runtime/market_data.rs server/src/runtime/reconcile.rs server/src/runtime/tests/startup_sync.rs server/src/runtime/tests/reconcile.rs server/src/runtime/tests/support.rs server/src/runtime/tests/user_data.rs server/src/runtime/tests/execution.rs server/src/assembly.rs docs/superpowers/plans/2026-04-15-market-data-health-scheduling.md
git commit -m "refactor(runtime): split market data health scheduling from recovery"
```

实现提交：`3220f4d`

### Task 3: 补齐 fresh tick 重置 deadline 的回归覆盖

**Files:**
- Modify: `server/src/runtime/tests/startup_sync.rs`
- Modify: `server/src/runtime/market_data.rs`
- Modify: `server/src/runtime/market_data_health.rs`

- [x] **Step 1: 先写回归测试，锁住 tick 成功后会重算 deadline**

新增至少这条测试：

```rust
#[tokio::test]
async fn fresh_tick_resets_market_data_health_deadline_before_timeout() {}
```

测试结构：

1. 发送第一笔 tick，拿到 `last_tick_at`
2. 把逻辑时钟推进到接近 timeout，但不越界
3. 再发送一笔新 tick
4. 再推进到“旧 deadline 已过、但新 deadline 未过”的时间
5. 断言 `market_data_stale_since` 仍然是 `None`

- [x] **Step 2: 运行定向测试，确认当前行为**

Run:

- `cargo test -p poise-server runtime::tests::startup_sync::fresh_tick_resets_market_data_health_deadline_before_timeout -- --exact --nocapture`

Expected:

- 如果测试失败，说明 dirty hook 或 deadline 重算还有缺口
- 如果测试直接通过，说明 Task 2 中提前落下的 dirty hook 已满足这条行为，Task 3 主要作为覆盖补齐

- [x] **Step 3: 仅在需要时补最小实现**

如果 Step 2 已经通过：

```rust
// 无需额外生产代码修改
```

如果 Step 2 失败，只在以下位置补最小修复：

- `server/src/runtime/market_data.rs`
- `server/src/runtime/market_data_health.rs`

- [x] **Step 4: 跑 Task 3 回归**

Run:

- `cargo test -p poise-server runtime::tests::startup_sync::fresh_tick_resets_market_data_health_deadline_before_timeout -- --exact --nocapture`
- `cargo test -p poise-server runtime::tests::startup_sync::background_health_check_marks_market_data_stale_without_follow_up_events -- --exact --nocapture`

Expected:

- 新 tick 会重置 stale deadline
- 无 follow-up tick 仍能按时标 stale

- [x] **Step 5: Commit**

```bash
git add server/src/runtime/tests/startup_sync.rs server/src/runtime/market_data.rs server/src/runtime/market_data_health.rs docs/superpowers/plans/2026-04-15-market-data-health-scheduling.md
git commit -m "feat(runtime): reschedule market data health on successful ticks"
```

实现提交：`6ec1848`

### Task 4: 全量验收并同步设计文稿

**Files:**
- Modify: `docs/superpowers/specs/2026-04-15-market-data-health-scheduling-design.md`
- Modify: `docs/superpowers/plans/2026-04-15-market-data-health-scheduling.md`

- [x] **Step 1: 校对实现后的接口名并回写设计文稿**

把设计文稿中的对应段落同步成最终边界：

```md
- `application` 暴露 `market_data_health_deadline(track_id)`
- `server` 直接使用 `ClockPort` 与 deadline 做调度
```

要求：

- 不改变设计的 owner 结论
- 只把接口形状改成实现后的真实版本

- [x] **Step 2: 跑完整回归**

Run:

- `cargo test -p poise-engine --no-run`
- `./target/debug/deps/poise_engine-237eca218a85b938 --nocapture`
- `cargo test -p poise-application --no-run`
- `./target/debug/deps/poise_application-7b2594c1d9b16a6d --nocapture`
- `cargo test -p poise-server --no-run`
- `./target/debug/deps/poise_server-cded5ca7f4175f44 --nocapture`

Expected:

- 三个 crate 全绿
- `background_health_check_marks_market_data_stale_without_follow_up_events` 继续通过
- 不再存在 recovery 负责 market data health 的测试假设

- [x] **Step 3: 更新计划勾选和提交记录**

把本 plan 中各 task 的：

- checkbox
- `实现提交：<填写 commit SHA>`

回写成真实状态。

- [x] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-04-15-market-data-health-scheduling-design.md docs/superpowers/plans/2026-04-15-market-data-health-scheduling.md
git commit -m "docs(runtime): sync market data health scheduling design and plan"
```

实现提交：`04a56f8`

## Plan Self-Review

- Spec coverage:
  - 独立 task、bounded sleep、deadline 查询、dirty 重算、startup seed、recovery 解耦、无 follow-up tick stale 行为，都有对应 task
  - 对 `ClockPort` 约束的实现化处理，落在 Task 1 和 Task 2
- Placeholder scan:
  - 无 `TODO` / `TBD`
  - 所有代码步骤都给了明确接口形状和命令
- Type consistency:
  - 统一使用 `market_data_health_deadline(...)`
  - `ClockPort` 统一由 runtime 直接持有
  - runtime 新任务统一命名为 `market_data_health_task`
