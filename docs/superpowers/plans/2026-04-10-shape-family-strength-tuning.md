# Shape Family 强度调整实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `inertial` 和 `responsive` 的曲线强度调回更可用的区间，保留现有名字和对称语义不变。

**Architecture:** 只改 `poise-core` 中 `ShapeFamily` 的精确指数参数，不改协议、配置或持久化边界。当前行为定义仍由 `core/src/strategy.rs` 和 `docs/superpowers/specs/2026-04-10-shape-family-symmetry-design.md` 共同持有；README 保持定性描述，不补精确数值。

**Tech Stack:** Rust workspace、Cargo tests、Markdown

---

## 文件与职责

- Modify: `core/src/strategy.rs`
  调整 `inertial` 和 `responsive` 的指数参数，并把半程位置的验收测试改成新强度。
- Modify: `docs/superpowers/specs/2026-04-10-shape-family-symmetry-design.md`
  更新精确参数和示例数值，保持设计文档与实现一致。
- Modify: `docs/superpowers/plans/2026-04-10-shape-family-strength-tuning.md`
  执行时勾选步骤，并在 task 完成后记录 commit SHA。

### Task 1: 调整曲线强度并同步当前设计文档

**Files:**
- Modify: `core/src/strategy.rs`
- Modify: `docs/superpowers/specs/2026-04-10-shape-family-symmetry-design.md`
- Modify: `docs/superpowers/plans/2026-04-10-shape-family-strength-tuning.md`
- Test: `core/src/strategy.rs`

- [x] **Step 1: 先写失败测试，锁住新的半程强度**

在 `core/src/strategy.rs` 的测试里把半程断言改成下面这组期望：

```rust
#[test]
fn stronger_shape_family_curves_have_tuned_inventory_separation_halfway_to_center() {
    let inertial = desired_exposure(
        95.0,
        &TrackConfig {
            shape_family: ShapeFamily::Inertial,
            ..neutral_config()
        },
    );
    let linear = desired_exposure(95.0, &neutral_config());
    let responsive = desired_exposure(
        95.0,
        &TrackConfig {
            shape_family: ShapeFamily::Responsive,
            ..neutral_config()
        },
    );

    assert_close(inertial.0, 5.10);
    assert_close(linear.0, 4.0);
    assert_close(responsive.0, 2.64);
    assert!(inertial.0 > linear.0);
    assert!(linear.0 > responsive.0);
}
```

- [x] **Step 2: 运行单测确认失败**

运行：

- `cargo test -p poise-core strategy::tests::stronger_shape_family_curves_have_tuned_inventory_separation_halfway_to_center -- --exact --nocapture`

预期：

- 失败，因为当前实现还是 `inertial = 1 / 3`、`responsive = 3.0`

实际执行说明：

- 失败点落在 `inertial` 半程值，旧实现给出 `6.3496`，和新目标 `5.10` 不符
- 说明测试已经准确锁住这次想调弱的曲线强度

- [x] **Step 3: 做最小实现，只调整精确指数**

在 `core/src/strategy.rs`：

- 把 `ShapeFamily::Inertial` 的指数从 `1.0 / 3.0` 调整为 `0.65`
- 把 `ShapeFamily::Responsive` 的指数从 `3` 调整为 `1.6`
- 同步更新 `desired_exposure` 上方注释里的公式

目标代码：

```rust
fn mirrored_shape_value(position: f64, shape_family: ShapeFamily) -> f64 {
    let magnitude = match shape_family {
        ShapeFamily::Linear => position.abs(),
        ShapeFamily::Inertial => position.abs().powf(0.65),
        ShapeFamily::Responsive => position.abs().powf(1.6),
    };

    if position >= 0.0 {
        -magnitude
    } else {
        magnitude
    }
}
```

在 `docs/superpowers/specs/2026-04-10-shape-family-symmetry-design.md`：

- 把精确参数更新成 `inertial = 0.65`、`responsive = 1.6`
- 把中性示例里的半程数值更新成：
  - `inertial`：`+5.10`
  - `linear`：`+4.00`
  - `responsive`：`+2.64`
- 保持定性解释不变，只把“太强”的口径改成“在可辨识和可用之间取平衡”

- [x] **Step 4: 跑核心回归和全量测试**

运行：

- `cargo test -p poise-core strategy::tests:: -- --nocapture`
- `cargo test --workspace --quiet`

预期：

- `poise-core` 的策略测试全部 PASS
- workspace 全量测试 PASS

实际执行说明：

- `cargo test -p poise-core strategy::tests:: -- --nocapture` PASS
- `cargo test --workspace --quiet` PASS，结果为 `658 passed, 3 ignored, 0 failed`

- [ ] **Step 5: Commit**

```bash
git add core/src/strategy.rs docs/superpowers/specs/2026-04-10-shape-family-symmetry-design.md docs/superpowers/plans/2026-04-10-shape-family-strength-tuning.md
git commit -m "feat(strategy): tune shape family strength"
```

执行后在本 task 末尾回写 commit SHA。
