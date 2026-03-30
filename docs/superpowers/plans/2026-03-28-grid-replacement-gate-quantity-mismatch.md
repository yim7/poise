# Grid Replacement Gate Quantity Mismatch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复同方向旧挂单在数量已经不匹配当前目标时仍被 replacement gate 长期保留，导致仓位无法继续调仓的问题。

**Architecture:** 在 `engine/src/reconciler.rs` 里把 replacement gate 的价格改善门槛限定到“同方向且数量按交易所步长等价”的场景；一旦数量失配，就直接走现有 `CancelAll + SubmitOrder` 重挂路径。用 `reconciler` 单测锁定行为，避免继续把过期挂单错误保留。

**Tech Stack:** Rust, cargo test, 现有 engine 测试框架

---

### Task 1: 修复数量失配时的 replacement gate

**Files:**
- Modify: `engine/src/reconciler.rs`
- Modify: `docs/superpowers/plans/2026-03-28-grid-replacement-gate-quantity-mismatch.md`
- Test: `engine/src/reconciler.rs`

- [x] **Step 1: 写失败测试，覆盖“同方向但数量失配时必须重挂”**

Run: `cargo test -p poise-engine same_side_quantity_differs -- --nocapture`
Observed: FAIL，当前实现错误返回 `NoOp`

- [x] **Step 2: 写最小实现，限制 replacement gate 只拦截同方向且数量等价的价格微调**

- [x] **Step 3: 运行目标单测确认转绿**

Run: `cargo test -p poise-engine same_side_quantity_differs -- --nocapture`
Observed: PASS

- [x] **Step 4: 运行 replacement gate 相关回归**

Run: `cargo test -p poise-engine replacement -- --nocapture`
Observed: PASS

- [x] **Step 5: 运行 engine 全量测试**

Run: `cargo test -p poise-engine`
Observed: PASS

- [x] **Step 6: 检查格式**

Run: `cargo fmt --all --check`
Observed: FAIL，仓库里已有无关未格式化文件 `server/src/config.rs`、`server/src/projector.rs`

Run: `cargo fmt --all -- engine/src/reconciler.rs`
Observed: PASS

- [x] **Step 7: 提交代码并回写 commit SHA**

Commit message: `fix: replace stale same-side orders when quantity drifts`
Commit SHA: `e073acb`
