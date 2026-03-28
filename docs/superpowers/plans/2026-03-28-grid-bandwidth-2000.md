# Grid Bandwidth 2000 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将 Binance 测试网示例配置里的 `btc-core` 网格区间调整为总带宽 `2000 USD`，并让 README 示例与配置保持一致。

**Architecture:** 不改运行时逻辑，只修改示例配置和文档。验收通过 `server/src/config.rs` 中的示例配置解析测试完成，确保 `configs/binance-testnet.toml` 的 `btc-core` 区间带宽固定为 `2000.0`，同时 README 示例同步更新。

**Tech Stack:** Rust, Cargo, TOML, Markdown

---

### Task 1: 收窄示例网格区间并同步文档

**Files:**
- Modify: `server/src/config.rs`
- Modify: `configs/binance-testnet.toml`
- Modify: `README.md`
- Modify: `docs/superpowers/plans/2026-03-28-grid-bandwidth-2000.md`
- Test: `server/src/config.rs`

- [x] **Step 1: 写失败测试，约束 `configs/binance-testnet.toml` 中 `btc-core` 的总带宽为 `2000.0`**

- [x] **Step 2: 运行单测确认红灯**

Run: `cargo test -p grid-server parses_binance_testnet_example_config -- --exact`
Expected: FAIL，原因是示例配置当前带宽不是 `2000.0`

- [x] **Step 3: 修改示例配置到当前价格附近的总带宽 `2000.0`**

要求：
- `upper_price - lower_price == 2000.0`
- 不修改其他网格参数

- [x] **Step 4: 同步更新 README 示例与说明**

- [x] **Step 5: 重新运行单测确认转绿**

Run: `cargo test -p grid-server parses_binance_testnet_example_config -- --exact`
Expected: PASS

- [x] **Step 6: 运行相关回归验证**

Run: `cargo test -p grid-server`

- [x] **Step 7: 提交代码并回写 commit SHA**

Run:
```bash
git add server/src/config.rs configs/binance-testnet.toml README.md docs/superpowers/specs/2026-03-28-grid-bandwidth-2000-design.md docs/superpowers/plans/2026-03-28-grid-bandwidth-2000.md
git commit -m "chore: narrow binance testnet grid bandwidth"
```

**Task 记录：**
- 状态：已完成
- 验收：
  - `cargo test -p grid-server config::tests::parses_binance_testnet_example_config -- --exact`
  - `cargo test -p grid-server`
  - `cargo fmt --all --check` 仍提示与本次任务无关的 `server/src/projector.rs` 既有格式差异
- 实现 commit SHA：`babcc9d`
