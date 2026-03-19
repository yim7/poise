# K4 Async Execution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让执行类命令先进入 `accepted`，再由服务端异步收口 `completed / failed / timed_out`，并把终态语义落到审计、恢复、WebSocket 与 TUI。

**Architecture:** 在 `service` 内核中增加 in-flight 执行状态与后台结果回投入口。`pause / resume` 保持本地即时完成；`cancel-all / flatten-now / shutdown-after-flatten` 改为先 accepted，再由 adapter 异步完成或服务端 timeout。TUI 继续消费统一的 `CommandAck` 终态，而不是本地推断最终语义。

**Tech Stack:** Rust, Tokio, SQLite, serde, ratatui

---

### Task 1: 为异步执行状态机补 failing tests

**Files:**
- Modify: `service/tests/kernel_flow.rs`
- Modify: `service/tests/persistence_recovery.rs`
- Test: `service/tests/kernel_flow.rs`

- [ ] **Step 1: 写失败测试，覆盖执行类命令先 accepted 再异步 ack**

补一条 `cancel-all` 或 `flatten-now` 测试，要求：

- `submit_command()` 返回 `accepted`
- 返回后读模型里先能看到 `pending_commands`
- 之后再从事件流里收到最终 `CommandAck`

- [ ] **Step 2: 运行定向测试，确认当前实现失败**

Run: `cargo test -p grid-platform-service --test kernel_flow async_execution`
Expected: FAIL，原因是当前实现会在同一轮里直接写入终态

- [ ] **Step 3: 写失败测试，覆盖服务端 timeout 与晚到结果不覆盖**

补测试要求：

- 命令 accepted 后若超时，服务端写入 `timed_out`
- adapter 晚到结果回投后，不覆盖已有 timeout 终态

- [ ] **Step 4: 写失败测试，覆盖执行类命令单飞与 adapter 失败**

补测试要求：

- 已有执行类命令 in-flight 时，第二个执行类命令立即收口为 `failed`
- adapter 返回错误时，命令写入 `failed` 且原因进入审计

- [ ] **Step 5: 运行定向测试，确认当前实现失败**

Run: `cargo test -p grid-platform-service --test kernel_flow timeout`
Expected: FAIL，原因是当前服务端没有异步 timeout 状态机

- [ ] **Step 6: 写失败测试，覆盖持久化审计幂等命中**

补测试要求：

- 命令终态即使不在 `recent_commands` 窗口中，也能通过 SQLite 审计命中幂等

- [ ] **Step 7: 运行定向测试，确认当前实现失败**

Run: `cargo test -p grid-platform-service --test persistence_recovery idempotent`
Expected: FAIL，原因是当前幂等只看内存窗口

### Task 2: 在内核中落地 in-flight 执行状态机

**Files:**
- Modify: `service/src/kernel.rs`
- Modify: `service/src/application.rs`
- Modify: `service/src/control_plane.rs`
- Modify: `service/src/lib.rs`
- Test: `service/tests/kernel_flow.rs`

- [ ] **Step 1: 为内核增加 in-flight 执行模型和结果回投命令**

增加仅服务端使用的执行状态结构，以及后台任务回投的 `EngineCommand` 分支。

- [ ] **Step 2: 让执行类命令只登记 accepted，不在接收命令时直接产出终态**

`cancel-all / flatten-now / shutdown-after-flatten` 在 `submit_command()` 中只：

- 写 `pending_commands`
- 设置 deadline
- 启动后台执行任务
- 返回 `CommandAccepted`

- [ ] **Step 3: 增加执行类命令单飞限制**

若已有执行类命令 in-flight，则新命令立即以 `failed` 终态收口，并写明冲突原因。

- [ ] **Step 4: 在回投路径中统一应用 `ExecutionOutcome`**

后台结果回投后，由单写者内核统一：

- 更新 runtime / open_orders / recent_fills
- 写 `recent_commands`
- 写 `last_command_ack_event`
- 生成 `EngineEvent::CommandAck`

- [ ] **Step 5: 增加服务端 timeout 处理**

在内核内增加 timeout 检查入口，确保 deadline 到达时写入 `timed_out`，并清理 in-flight。

- [ ] **Step 6: 运行服务端定向测试**

Run: `cargo test -p grid-platform-service --test kernel_flow`
Expected: PASS

### Task 3: 提升幂等与审计恢复

**Files:**
- Modify: `service/src/storage.rs`
- Modify: `service/src/kernel.rs`
- Test: `service/tests/persistence_recovery.rs`

- [ ] **Step 1: 增加按 `command_id` 查询审计记录的存储能力**

让存储层可按 `command_id` 读取命令终态、原因和关联字段。

- [ ] **Step 2: 幂等命中时先查内存窗口，再查 SQLite 审计**

命中持久化终态时，返回同一语义的 `CommandAccepted + CommandAck` 结果，不重新执行。

- [ ] **Step 3: 运行持久化定向测试**

Run: `cargo test -p grid-platform-service --test persistence_recovery`
Expected: PASS

### Task 4: 让 TUI 明确消费服务端终态

**Files:**
- Modify: `tui/src/store.rs`
- Modify: `tui/tests/local_paper_e2e.rs`
- Test: `tui/tests/local_paper_e2e.rs`

- [ ] **Step 1: 调整 TUI 对执行命令的期望**

确保执行类命令依赖服务端 `CommandAck` 收口终态，而不是本地立即推断最终状态。

- [ ] **Step 2: 补本地 E2E**

覆盖：

- accepted 后进入 timeline
- 最终 ack 驱动状态完成
- failure / timeout 原因来自服务端 ack
- timeout 与重连后仍能恢复服务端终态

- [ ] **Step 3: 运行 TUI 定向测试**

Run: `cargo test -p grid-platform-tui --test local_paper_e2e`
Expected: PASS

### Task 5: 全量验证与文档回写

**Files:**
- Modify: `docs/plan.md`
- Modify: `../TODO.md`
- Test: workspace

- [ ] **Step 1: 全量运行格式化与测试**

Run: `cargo fmt`
Expected: PASS

Run: `cargo test`
Expected: PASS

- [ ] **Step 2: 回写 K4 状态**

在 `docs/plan.md` 和 `../TODO.md` 中更新 K4 的完成情况与剩余风险。

- [ ] **Step 3: 做一次代码 review**

重点检查：

- 执行终态是否仍有同步改快照路径
- timeout 是否由服务端生成
- 晚到结果是否会覆盖终态
- 幂等是否跨越内存窗口
