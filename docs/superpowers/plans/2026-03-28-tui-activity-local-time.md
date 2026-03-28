# TUI Activity Local Time Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 `grid-tui` 的 `Activity` 列表按本机时区显示时间戳，同时保持协议和服务端输出不变。

**Architecture:** 只修改 `tui` 渲染层。`activity.ts` 仍作为 RFC3339 字符串从服务端传到客户端，在 `tui/src/views/instance.rs` 渲染时解析并转换成本地时区；解析失败时回退原值，避免把 UI 和协议耦合到一起。

**Tech Stack:** Rust, chrono, ratatui, cargo test

---

### Task 1: 在 TUI 渲染层显示本地时区 activity 时间

**Files:**
- Modify: `tui/Cargo.toml`
- Modify: `tui/src/views/instance.rs`
- Modify: `docs/superpowers/specs/2026-03-28-tui-activity-local-time-design.md`
- Modify: `docs/superpowers/plans/2026-03-28-tui-activity-local-time.md`
- Test: `tui/src/views/instance.rs`

- [x] **Step 1: 写失败测试，覆盖合法 activity 时间戳会被本地格式化**

- [x] **Step 2: 运行单测确认红灯**

Run: `cargo test -p grid-tui views::instance::tests::renders_activity_timestamp_in_local_time -- --exact`
Expected: FAIL，原因是当前仍原样显示 `2026-03-26T10:01:00Z`

- [x] **Step 3: 补充非法 activity 时间戳回退原字符串测试**

- [x] **Step 4: 运行相关单测确认行为符合预期**

Run: `cargo test -p grid-tui activity_timestamp -- --nocapture`
Expected:
- 合法时间戳用例先 FAIL，补齐实现后 PASS
- 非法时间戳用例保持原字符串显示

- [x] **Step 5: 添加最小实现**

实现要求：
- `tui` 新增 `chrono` 依赖
- 只在 `Activity` 列表渲染时转换时间
- 解析失败回退原字符串

- [x] **Step 6: 运行相关单测确认转绿**

Run: `cargo test -p grid-tui activity_timestamp -- --nocapture`

- [x] **Step 7: 运行 TUI 回归测试**

Run: `cargo test -p grid-tui`

- [x] **Step 8: 提交代码并回写 commit SHA**

Run:
```bash
git add tui/Cargo.toml tui/src/views/instance.rs docs/superpowers/specs/2026-03-28-tui-activity-local-time-design.md docs/superpowers/plans/2026-03-28-tui-activity-local-time.md
git commit -m "feat: render activity timestamps in local time"
```

**Task 记录：**
- 状态：已完成
- 验收：
  - `cargo test -p grid-tui views::instance::tests::renders_activity_timestamp_in_local_time -- --exact`
  - `cargo test -p grid-tui activity_timestamp -- --nocapture`
  - `cargo test -p grid-tui`
- 实现 commit SHA：待回写
