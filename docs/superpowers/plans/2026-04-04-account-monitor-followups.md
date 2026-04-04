# 账户监控后续收敛 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 收紧账户监控实现的两个设计风险：让 `AccountMonitor` 不再依赖过宽的 `ExchangePort`，并让 TUI 账户摘要刷新失败时不阻断 track 主链路。

**Architecture:** 这次 follow-up 只做边界收紧，不扩任何新功能。第一部分把账户监控依赖缩成只读账户摘要的最小 port，让 `AccountMonitor` 不再携带一整套交易执行接口；第二部分把 TUI 的账户摘要读取改成 best-effort，保留 `/account -> /tracks -> /tracks/:id` 的顺序，但账户摘要失败只让面板进入 `unavailable`，不让 dashboard 和详情加载失败。

**Tech Stack:** Rust workspace, Cargo, Tokio, Axum, Ratatui, Reqwest, Serde

---

## File Structure

### 重点修改文件

- `engine/src/ports.rs`：新增最小账户摘要读取 port，保留 `ExchangePort` 的现有职责
- `exchanges/binance/src/adapter.rs`：实现新的账户摘要只读 port
- `server/src/account_monitor.rs`：改依赖最小账户摘要 port，删除与下单、撤单无关的占位实现
- `server/src/assembly.rs`：装配 `AccountMonitor` 时传入新的账户摘要 port
- `server/src/http.rs`：仅回归验证，无需改协议形状
- `server/src/runtime.rs`：仅回归验证，无需改调度边界
- `tui/src/main.rs`：把账户摘要 bootstrap / resync 改成 best-effort
- `tui/src/app.rs`：如需要，增加清空账户摘要的辅助方法
- `tui/src/views/account_panel.rs`：确认 `None` 时稳定显示 `unavailable`

### 测试落点

- `server/src/account_monitor.rs`
- `server/src/runtime.rs`
- `tui/src/main.rs`
- `tui/src/views/dashboard.rs`

### 实施约束

- 每个 task 先按 `@superpowers/test-driven-development` 写失败测试，再写实现
- 每个 task 验收通过后必须立即 `git add`、`git commit`，并把 commit SHA 回写到本计划
- 未完成当前 task 的提交和计划回写，不得开始下一个 task
- task 完成前按 `@superpowers/verification-before-completion` 跑对应回归
- 不在这次 follow-up 中改动账户口径、阈值、UI 文案或协议结构

---

### Task 1: 缩小 AccountMonitor 的交易所依赖边界

**Files:**
- Modify: `engine/src/ports.rs`
- Modify: `exchanges/binance/src/adapter.rs`
- Modify: `server/src/account_monitor.rs`
- Modify: `server/src/assembly.rs`
- Test: `cargo test -p poise-server account_monitor::tests::marks_equity_below_zero_as_critical -- --nocapture`
- Test: `cargo test -p poise-server runtime::tests::account_monitor_task_triggers_immediate_refresh_and_periodic_refresh -- --nocapture`

- [x] **Step 1: 先写失败测试，固定 AccountMonitor 只依赖账户摘要能力**

要求：
- 在 `server/src/account_monitor.rs` 增加一个只实现账户摘要读取的最小 fake source，用它构造 `AccountMonitor`
- 测试要明确表明：账户监控不需要提交订单、撤单、查询仓位这些能力也能工作
- 保留现有 `AccountMonitor` 风险计算和通知行为测试，确保只是接口缩窄，不是行为变化

- [x] **Step 2: 运行定向测试，确认当前实现仍要求完整 ExchangePort**

Run:
`cargo test -p poise-server account_monitor::tests::marks_equity_below_zero_as_critical -- --nocapture`
`cargo test -p poise-server runtime::tests::account_monitor_task_triggers_immediate_refresh_and_periodic_refresh -- --nocapture`

Expected:
- 测试失败或编译失败
- 失败原因明确指向：
  - `AccountMonitor` 仍要求完整 `ExchangePort`
  - 只实现账户摘要读取的 fake 还无法注入

- [x] **Step 3: 实现最小账户摘要读取 port**

要求：
- 在 `engine/src/ports.rs` 新增最小接口，例如：

```rust
#[async_trait]
pub trait AccountSummaryPort: Send + Sync {
    async fn get_account_summary(&self) -> Result<AccountSummarySnapshot>;
}
```

- `ExchangePort` 继续保留现有方法，但不再是 `AccountMonitor` 的依赖
- `exchanges/binance/src/adapter.rs` 同时实现 `ExchangePort` 和新的 `AccountSummaryPort`
- `server/src/account_monitor.rs` 改为持有 `Arc<dyn AccountSummaryPort>`
- 删除 `UnsupportedAccountSummaryExchange` 这类为了满足完整 `ExchangePort` 而存在的大段占位实现，替换成只实现最小 port 的 unavailable source
- `server/src/assembly.rs` 在装配 `AccountMonitor` 时传入新的账户摘要 port，不改 runtime / HTTP / WS 的使用方式

- [x] **Step 4: 跑 server 回归**

Run:
`cargo test -p poise-server account_monitor::tests::marks_equity_below_zero_as_critical -- --nocapture`
`cargo test -p poise-server runtime::tests::account_monitor_task_triggers_immediate_refresh_and_periodic_refresh -- --nocapture`
`cargo test -p poise-server`

Expected:
- `AccountMonitor` 行为测试仍通过
- runtime 轮询测试仍通过
- `poise-server` 全量测试通过

- [x] **Step 5: 提交并回写 SHA**

```bash
git add engine/src/ports.rs exchanges/binance/src/adapter.rs server/src/account_monitor.rs server/src/assembly.rs
git commit -m "refactor: narrow account monitor exchange dependency"
```

Task 1 code commit:
`d51cdfbdd465cfa169d9a620da57696029e0138b`

---

### Task 2: 让 TUI 账户摘要刷新失败时降级而不阻断主读链路

**Files:**
- Modify: `tui/src/main.rs`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/views/account_panel.rs`
- Test: `cargo test -p poise-tui tests::load_initial_state_keeps_tracks_when_account_summary_request_fails -- --nocapture`
- Test: `cargo test -p poise-tui tests::sync_projected_state_keeps_tracks_when_account_summary_request_fails -- --nocapture`
- Test: `cargo test -p poise-tui views::dashboard::tests::renders_unavailable_account_panel_when_summary_is_missing -- --nocapture`

- [ ] **Step 1: 先写失败测试，固定账户摘要失败时的降级行为**

要求：
- 在 `tui/src/main.rs` 增加启动测试，固定 `/account` 返回错误时：
  - `/tracks` 和 `/tracks/:id` 仍会继续请求
  - `App` 仍成功加载 dashboard 和选中 track 详情
  - `account_summary` 保持为空
- 在 `tui/src/main.rs` 增加 resync 测试，固定 `sync_projected_state()` 中账户摘要失败不会让 track 刷新失败
- 在 `tui/src/views/dashboard.rs` 或 `tui/src/views/account_panel.rs` 固定 `summary == None` 时渲染 `unavailable`

- [ ] **Step 2: 运行定向测试，确认当前实现把账户摘要失败当成整体失败**

Run:
`cargo test -p poise-tui tests::load_initial_state_keeps_tracks_when_account_summary_request_fails -- --nocapture`
`cargo test -p poise-tui tests::sync_projected_state_keeps_tracks_when_account_summary_request_fails -- --nocapture`
`cargo test -p poise-tui views::dashboard::tests::renders_unavailable_account_panel_when_summary_is_missing -- --nocapture`

Expected:
- 启动或 resync 测试失败
- 失败原因明确指向：
  - `load_initial_state()` 仍然对 `get_account_summary()` 使用 `?`
  - `sync_projected_state()` 仍然对账户摘要失败直接返回错误

- [ ] **Step 3: 实现账户摘要 best-effort 刷新**

要求：
- 在 `tui/src/main.rs` 抽出一个 best-effort 账户摘要读取辅助，例如：

```rust
async fn load_account_summary_best_effort(client: &ApiClient) -> Option<AccountSummaryView>
```

- `load_initial_state()` 继续维持 `/account -> /tracks -> /tracks/:id` 的顺序，但账户摘要失败只记录状态并继续
- `sync_projected_state()` 账户摘要失败时仍刷新 track 列表、详情和 diagnostics，最后把 `account_summary` 置为 `None`
- 如有必要，在 `tui/src/app.rs` 增加 `clear_account_summary()`
- `tui/src/views/account_panel.rs` 继续负责 `None => unavailable` 的唯一渲染分支，不把 fallback 判断扩散到 `dashboard.rs`

- [ ] **Step 4: 跑 TUI 和跨包回归**

Run:
`cargo test -p poise-tui`
`cargo test -p poise-protocol -p poise-binance -p poise-storage -p poise-server -p poise-tui`

Expected:
- `poise-tui` 全量测试通过
- 跨包回归通过
- 账户摘要 HTTP 失败时，track 主链路仍可启动、刷新和接收 WS 更新

- [ ] **Step 5: 提交并回写 SHA**

```bash
git add tui/src/main.rs tui/src/app.rs tui/src/views/account_panel.rs
git commit -m "refactor: make tui account summary refresh best effort"
```

Task 2 code commit:
`<pending>`
