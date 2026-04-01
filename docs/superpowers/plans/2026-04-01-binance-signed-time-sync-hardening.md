# Binance 签名时间同步加固 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 Binance signed REST 在本机时钟偏移较大时持续踩中 `recvWindow` 的问题，避免 `openOrders` / `positionRisk` 预检查失败把恢复流程长期卡住。

**Architecture:** 保持时间偏移逻辑集中在 Binance REST 签名层，不把校时分散到业务调用点。实现分两部分：先用同一组测试锁住主动校时与更宽时间窗口，再补充失败日志和整体验证，确保线上可以判断是时间窗问题还是其他网络问题。

**Tech Stack:** Rust, reqwest, tokio, Binance REST client tests

---

### Task 1: 加固 Binance signed 请求的时间同步

**Files:**
- Modify: `exchanges/binance/src/rest.rs`
- Modify: `docs/superpowers/plans/2026-04-01-binance-signed-time-sync-hardening.md`

- [x] **Step 1: 写 failing tests**

已补两类测试：
- 有过成功校时且校时已过期时，signed 请求应先主动刷新时间偏移再访问业务接口。
- signed 请求使用更宽的 `recvWindow`。

- [x] **Step 2: 运行测试确认红灯**

Run: `cargo test -p poise-binance rest::tests:: -- --nocapture`

Observed:
- 新增主动校时测试因 `openOrders` 提前消费了 `/fapi/v1/time` 返回体而失败
- `recvWindow` 与签名值相关断言失败

- [x] **Step 3: 写最小实现并复跑**

实现内容：
- `DEFAULT_RECV_WINDOW_MS` 从 `5000` 调整到 `10000`
- 记录最近一次成功校时的时间
- 对 signed 请求在校时过期时先主动调用 `sync_server_time_offset()`
- 保留现有 `-1021 -> sync + retry` 兜底

Run: `cargo test -p poise-binance rest::tests:: -- --nocapture`

Expected: PASS

- [x] **Step 4: 提交 Task 1**

```bash
git add exchanges/binance/src/rest.rs docs/superpowers/plans/2026-04-01-binance-signed-time-sync-hardening.md
git commit -m "fix(binance): harden signed request time sync"
```

已完成：`c712e7b`

### Task 2: 补全 signed 请求失败诊断并做整体验证

**Files:**
- Modify: `exchanges/binance/src/rest.rs`
- Modify: `docs/superpowers/plans/2026-04-01-binance-signed-time-sync-hardening.md`

- [x] **Step 1: 补充日志与错误上下文**

要求：
- `request GET ... failed` 需要保留足够上下文，能区分 `-1021`、网络错误、反序列化错误
- 不泄露 API key / secret / signature

- [x] **Step 2: 运行完整验证**

Run:
- `cargo test -p poise-binance -- --nocapture`
- `cargo test -p poise-server runtime::tests:: -- --nocapture`
- `cargo test -p poise-server effect_worker::tests:: -- --nocapture`

Expected:
- Binance crate 全绿
- server 相关恢复路径测试不回归

已完成：
- `cargo test -p poise-binance -- --nocapture`
- `cargo test -p poise-server runtime::tests:: -- --nocapture`
- `cargo test -p poise-server effect_worker::tests:: -- --nocapture`

- [ ] **Step 3: 更新任务清单并提交 Task 2**

在本文件里回写：
- 每个 task 的完成状态
- 对应 commit SHA

```bash
git add exchanges/binance/src/rest.rs docs/superpowers/plans/2026-04-01-binance-signed-time-sync-hardening.md
git commit -m "chore(binance): improve signed request diagnostics"
```

已完成：`60c6ff4`
