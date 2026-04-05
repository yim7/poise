# Account Margin Guard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为共享交易账号增加启动预检和运行时保证金熔断，避免账号容量不足时持续发出会增加风险的订单。

**Architecture:** 账号容量信息通过 `ExchangePort` 进入 server runtime，由 server 维护账号级 margin guard；engine 只接收抽象后的“是否允许继续增加风险”输入，并在 reconcile 阶段转成 `RiskDenied`。`-2019` 的交易所错误只在 adapter/effect worker/server 边界内处理，不泄漏进 planning 细节。

**Tech Stack:** Rust, tokio, axum, reqwest, rusqlite, Binance USDⓈ-M Futures API

---

## Task 1: 定义账号容量抽象

**Files:**
- Modify: `engine/src/ports.rs` — 增加账号容量结构和 exchange 读取接口
- Modify: `engine/src/runtime.rs` — 增加 track 可见的账号容量约束输入
- Modify: `engine/src/snapshot.rs` — 仅在确有需要时持久化最小约束视图，不持久化完整账号 guard
- Test: 各文件内 `#[cfg(test)]` 编译和序列化测试

- [x] **Step 1: 写 failing test，覆盖 snapshot 序列化新字段**

在 `engine/src/runtime.rs` 或 `engine/src/snapshot.rs` 增加一个测试，构造带 margin guard 的 snapshot，序列化再反序列化，断言字段完整保留。

再补一条旧版 JSON 反序列化测试，确认缺少新字段时会走安全默认值，而不是破坏已有持久化数据加载。

- [x] **Step 2: 运行单测，确认当前失败**

Run: `cargo test -p poise-engine margin_guard -- --nocapture`
Expected: 编译失败或测试失败，因为相关结构尚不存在。

- [x] **Step 3: 增加账号容量和 guard 结构**

在 `engine/src/ports.rs` 增加：

```rust
pub struct AccountCapacitySnapshot {
    pub venue: Venue,
    pub available_balance: f64,
    pub total_wallet_balance: f64,
    pub max_increase_notional: f64,
    pub observed_at: DateTime<Utc>,
}
```

在 `ExchangePort` 增加：

```rust
async fn get_account_capacity_snapshot(&self, instrument: &Instrument) -> Result<AccountCapacitySnapshot>;
```

在 `engine/src/runtime.rs` 或 `core/src/risk.rs` 增加一个面向 reconcile 的最小约束视图，例如：

```rust
pub struct AccountCapacityConstraint {
    pub increase_blocked: bool,
    pub blocked_reason: Option<String>,
    pub max_increase_notional: Option<f64>,
}
```

要求：
- server 持有完整 `AccountMarginGuard`
- engine 只接收 `AccountCapacityConstraint`
- 不把 `snapshot`、`blocked_at` 这类账号级恢复细节直接挂进 engine runtime

- [x] **Step 4: 运行单测，确认通过**

Run: `cargo test -p poise-engine margin_guard -- --nocapture`
Expected: PASS

- [x] **Step 5: 提交**

```bash
git add engine/src/ports.rs engine/src/runtime.rs engine/src/snapshot.rs
git commit -m "feat: add account margin guard types"
```

Task 1 code commit: `e03d1ec`

## Task 2: Binance 适配层提供账号容量快照

**Files:**
- Modify: `exchanges/binance/src/types.rs` — 增加账户响应结构
- Modify: `exchanges/binance/src/rest.rs` — 增加账户 REST 查询
- Modify: `exchanges/binance/src/adapter.rs` — 实现 `get_account_capacity_snapshot`
- Modify: `server/src/main.rs` 及其他测试内 `ExchangePort` 假实现 — 补齐新 trait 方法
- Test: `exchanges/binance/src/rest.rs`, `exchanges/binance/src/adapter.rs`

- [x] **Step 1: 写 failing test，验证 adapter 能返回账号容量快照**

在 `exchanges/binance/src/adapter.rs` 增加测试，mock Binance 账户接口响应，断言：
- `available_balance`
- `total_wallet_balance`
- `max_increase_notional`

都被正确映射。

- [x] **Step 2: 运行单测，确认当前失败**

Run: `cargo test -p poise-binance account_capacity_snapshot -- --nocapture`
Expected: FAIL，因为还没有账户接口和解析实现。

- [x] **Step 3: 增加 Binance 账户查询**

在 `rest.rs` 增加一个新方法，例如：

```rust
pub async fn get_account_capacity_snapshot(&self, symbol: &str) -> Result<AccountCapacitySnapshot>
```

第一版可以使用单个 Binance 账户接口响应，内部完成：
- 读取 `availableBalance`
- 读取 `totalWalletBalance`
- 读取 symbol 对应 leverage 或等价容量字段
- 计算 `max_increase_notional`

不要把 Binance 原始 JSON 直接暴露到 engine。

- [x] **Step 4: 实现 adapter 接口并补齐 mock server 测试**

在 `adapter.rs` 的 `impl ExchangePort for BinanceAdapter` 里实现新方法。

同时全局搜索 `impl ExchangePort for`，把 server 内的 fake exchange / mock exchange 一并补齐，否则新增 trait 方法后会导致整个 workspace 编译失败。

- [x] **Step 5: 运行单测，确认通过**

Run: `cargo test -p poise-binance account_capacity_snapshot -- --nocapture`
Expected: PASS

- [x] **Step 6: 提交**

```bash
git add exchanges/binance/src/types.rs exchanges/binance/src/rest.rs exchanges/binance/src/adapter.rs
git commit -m "feat: expose binance account capacity snapshot"
```

Task 2 code commit: `f6b7bab`

## Task 3: 启动时执行账号容量预检

**Files:**
- Modify: `server/src/assembly.rs` — runtime 组装时拉取账号容量并预检
- Modify: `server/src/runtime.rs` — 初始化账号级 margin guard 状态
- Test: `server/src/assembly.rs`

- [x] **Step 1: 写 failing test，账号容量小于配置最大仓位时启动失败**

在 `server/src/assembly.rs` 增加测试：
- mock exchange 返回 `max_increase_notional = 5000`
- track `budget.max_notional = 20000`
- 断言 assembly/start 返回错误，错误信息包含 `insufficient account margin`

- [x] **Step 2: 运行单测，确认当前失败**

Run: `cargo test -p poise-server startup_margin_preflight -- --nocapture`
Expected: FAIL，因为启动阶段还没有做这个检查。

- [x] **Step 3: 在 assembly 加预检**

按账号读取一次容量快照，并对每个 track 做：

```rust
if track.budget().max_notional > snapshot.max_increase_notional {
    bail!("insufficient account margin for configured max_notional");
}
```

第一版直接失败，不做自动降级。

- [x] **Step 4: 初始化 runtime 的账号级 guard**

把启动时读取到的快照放进 runtime 的账号 guard 存储，供后续 reconcile 和 `-2019` 恢复使用。

同时明确一条实现约束：server 内保存完整 `AccountMarginGuard`，传给 engine 的是按当前 guard 派生出的 `AccountCapacityConstraint`，不要把完整 guard 直接塞进 track snapshot。

- [x] **Step 5: 运行单测，确认通过**

Run: `cargo test -p poise-server startup_margin_preflight -- --nocapture`
Expected: PASS

- [x] **Step 6: 提交**

```bash
git add server/src/assembly.rs server/src/runtime.rs
git commit -m "feat: validate account margin on startup"
```

Task 3 code commit: `23de335`

## Task 4: `-2019` 触发账号级风险增加熔断

**Files:**
- Modify: `server/src/effect_worker.rs` — 识别 `-2019`，触发 guard 和账户重同步
- Modify: `server/src/runtime.rs` — 维护账号级 guard 状态和刷新逻辑
- Modify: `server/src/notifications.rs` 或相关通知代码 — 暴露 attention required
- Test: `server/src/runtime.rs`, `server/src/effect_worker.rs`

- [x] **Step 1: 写 failing test，`-2019` 后 guard 激活**

在 `server/src/runtime.rs` 或 `server/src/effect_worker.rs` 增加测试：
- mock exchange `submit_order` 返回 `code=-2019`
- 断言 runtime 里的账号 guard 被置为 `increase_blocked=true`
- 断言后续状态带 `attention_required`

- [x] **Step 2: 运行单测，确认当前失败**

Run: `cargo test -p poise-server insufficient_margin_guard -- --nocapture`
Expected: FAIL

- [x] **Step 3: 在 effect worker 识别 `-2019`**

不要做字符串模糊匹配，优先提取 Binance 错误码；如果 Binance 存在多个“保证金不足”拒单码，要在 adapter 层统一映射成一个内部原因，只在一处维护。

- [x] **Step 4: 激活账号级 guard 并触发容量重同步**

处理流程：
- 当前 effect 标记失败
- guard 置为 `increase_blocked`
- 保存 `blocked_reason = "insufficient_margin"`
- 拉取新的 `AccountCapacitySnapshot`
- 快照证明恢复前，不允许新的风险增加单

这里更新的是 server 持有的完整账号级 guard；后续 reconcile 只能读取它投影出来的最小约束视图。

- [x] **Step 5: 运行单测，确认通过**

Run: `cargo test -p poise-server insufficient_margin_guard -- --nocapture`
Expected: PASS

- [x] **Step 6: 提交**

```bash
git add server/src/effect_worker.rs server/src/runtime.rs server/src/notifications.rs
git commit -m "feat: trip account margin guard on insufficient margin"
```

Task 4 code commit: `0214a40`

注：本 task 完成的是 server-side guard 激活和容量快照刷新；对外 `attention_required` 的 read model / projector 投影跟 Task 5 的 reconcile 约束一起闭合，避免状态规则拆散到两处实现。

## Task 5: reconcile 阶段阻止新的风险增加单

**Files:**
- Modify: `core/src/risk.rs` — 增加账号容量限制输入
- Modify: `engine/src/reconciler.rs` — guard 激活时返回 `RiskDenied`
- Modify: `engine/src/manager.rs` — 把 runtime guard 状态传入 reconcile
- Modify: `server/src/projector.rs` — 活动和状态文案展示 `RiskDenied`
- Test: `core/src/risk.rs`, `engine/src/reconciler.rs`, `server/src/runtime.rs`

- [x] **Step 1: 写 failing test，guard 激活时不会再产出风险增加单**

在 `engine/src/reconciler.rs` 增加测试：
- 当前仓位 `1.0`
- 目标仓位 `5.0`
- margin guard 激活
- 断言返回 `RiskDenied`
- 断言 `suppress_execution = true`
- 断言它与现有 `RiskDenied` 路径保持同样的 `suppress_execution` 语义，不引入第二套阻断规则

- [x] **Step 2: 写 failing test，`reduce_only` 路径仍然允许**

构造当前仓位 `5.0`，目标 `2.0`，guard 激活：
- 断言 reconcile 不会被 margin guard 拦截

- [x] **Step 3: 运行单测，确认当前失败**

Run: `cargo test -p poise-engine margin_guard_reconcile -- --nocapture`
Expected: FAIL

- [x] **Step 4: 扩展 risk/reconcile 输入**

把账号容量限制建模为明确输入，例如：

```rust
pub struct AccountCapacityConstraint {
    pub increase_blocked: bool,
    pub max_increase_notional: Option<f64>,
}
```

在 reconcile 中只拦“增加绝对风险”的情况；减仓、平仓、`reduce_only` 继续允许。

不要让 reconcile 直接依赖 server 的完整 `AccountMarginGuard`。reconcile 只消费 `AccountCapacityConstraint`，保持 engine/server 边界单向清晰。

- [x] **Step 5: 把 `RiskDenied` 原因投影到 read model**

让 UI/活动流能看到明确原因，例如 `risk denied: insufficient account margin`。

- [x] **Step 6: 运行单测，确认通过**

Run: `cargo test -p poise-engine margin_guard_reconcile -- --nocapture`
Expected: PASS

- [x] **Step 7: 运行 server 回归测试**

Run: `cargo test -p poise-server insufficient_margin_guard -- --nocapture`
Expected: PASS

- [x] **Step 8: 提交**

```bash
git add core/src/risk.rs engine/src/reconciler.rs engine/src/manager.rs server/src/projector.rs server/src/runtime.rs
git commit -m "feat: deny risk-increasing orders when account margin is blocked"
```

Task 5 code commit: `fd23ccf`

注：实现时没有把这条规则下沉到 `core/src/risk.rs`，而是放在 `engine/src/reconciler.rs`。原因是它消费的是 engine 侧的 `AccountCapacityConstraint` 投影，不是 core 层独立可用的纯风控输入；这样可以避免把 server-side guard 语义继续往 core 泄漏。

## Task 6: 全量验证

**Files:**
- Modify: 无
- Test: 全量相关测试

- [x] **Step 1: 运行 engine 测试**

Run: `cargo test -p poise-engine`
Expected: PASS

- [x] **Step 2: 运行 Binance 适配层测试**

Run: `cargo test -p poise-binance`
Expected: PASS

- [x] **Step 3: 运行 server 测试**

Run: `cargo test -p poise-server`
Expected: PASS

- [x] **Step 4: 如有需要，补一次手工验收**

在 testnet 配置下使用一个明显不足以支撑 `max_notional` 的账号启动：
- 启动前预检失败

或在运行中人为制造 `-2019`：
- 只出现一次交易所拒单
- 后续变成 `RiskDenied`/`attention_required`
- `reduce_only` 仍可继续执行
- engine 侧看不到账号级 `snapshot` / `blocked_at` 细节，只消费最小约束视图

本次没有额外执行手工验收，因为以上场景已经分别被 `startup_margin_preflight`、`insufficient_margin_guard` 和 `margin_guard_reconcile` 自动测试覆盖。

- [x] **Step 5: 提交**

```bash
git add core/src/risk.rs engine/src/reconciler.rs engine/src/manager.rs engine/src/ports.rs engine/src/runtime.rs engine/src/snapshot.rs exchanges/binance/src/types.rs exchanges/binance/src/rest.rs exchanges/binance/src/adapter.rs server/src/assembly.rs server/src/runtime.rs server/src/effect_worker.rs server/src/notifications.rs server/src/projector.rs server/src/main.rs
git commit -m "test: verify account margin guard end to end"
```

Task 6 final fix commit: `fad5c0f`
