# Shape Family 对称实现计划

> **执行说明：** 执行本计划时必须使用 `subagent-driven-development` 或 `executing-plans`。步骤用 `- [ ]` 记录，task 验收通过后立即提交，并把 commit SHA 回写到本文件。

**目标：** 把 `shape_family` 改成围绕价格带中点对称的控仓曲线，并把用户可见名字统一成 `linear / inertial / responsive`，让接口名本身表达控仓行为，而不是继续暴露会误导的几何术语。

**架构：** `poise-core` 负责唯一的曲线数学语义和精确曲率参数；配置、协议和 TUI 只暴露行为名，不携带几何解释。README 和历史设计文档只保留定性行为说明和图示，不再重复 `p = 1 / 3`、`p = 3.0` 这类实现细节。

**技术栈：** Rust workspace、serde、TOML、Cargo tests、Markdown

---

## 文件与职责

- Modify: `core/src/strategy.rs`
  重写中点对称的 `desired_exposure` 公式，并把 `ShapeFamily` 改成 `Linear / Inertial / Responsive`。
- Modify: `application/src/track_definition.rs`
  保持默认 `ShapeFamily::Linear`，更新使用新枚举名的构造和测试。
- Modify: `protocol/src/lib.rs`
  更新 `ShapeFamily` 协议枚举、`Display` 输出和 JSON 解析测试。
- Modify: `server/src/projector.rs`
  把 core 枚举映射到 protocol 枚举。
- Modify: `server/src/config.rs`
  更新 TOML 解析测试，接受 `inertial / responsive`，拒绝 `concave / convex`。
- Modify: `engine/src/manager.rs`
  锁住 `shape_family` 变化会触发 restore revision 不匹配的恢复边界。
- Modify: `tui/src/main.rs`
  更新 e2e 配置夹具里的 `shape_family` 值。
- Modify: `README.md`
  更新配置说明，只保留行为语义。
- Modify: `docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md`
  增加历史说明并指向新设计，不再复制当前定义。
- Modify: `docs/superpowers/specs/assets/2026-04-10-shape-family-symmetry-examples.svg`
  把示意图标签更新成 `inertial / responsive`。
- Modify: `docs/superpowers/plans/2026-04-10-shape-family-symmetry.md`
  执行时勾选步骤，并在每个完成 task 后记录 commit SHA。

### Task 1: 同一次提交完成新名字、对称公式和恢复边界

**Files:**
- Modify: `core/src/strategy.rs`
- Modify: `application/src/track_definition.rs`
- Modify: `protocol/src/lib.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/config.rs`
- Modify: `engine/src/manager.rs`
- Modify: `tui/src/main.rs`
- Test: `core/src/strategy.rs`
- Test: `application/src/track_definition.rs`
- Test: `protocol/src/lib.rs`
- Test: `server/src/config.rs`
- Test: `engine/src/manager.rs`

- [x] **Step 1: 先写失败测试，锁住接口、行为和恢复边界**

在 `server/src/config.rs` 增加或替换成下面两条测试：

```rust
#[test]
fn parses_new_shape_family_names() {
    let config = parse_config(
        r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 4.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
shape_family = "inertial"
"#,
    )
    .unwrap();

    let track = &config.tracks[0];
    assert_eq!(track.shape_family, Some(ShapeFamily::Inertial));
}

#[test]
fn rejects_legacy_shape_family_names_with_migration_hint() {
    let error = parse_config(
        r#"
[exchange]
venue = "binance"

[[tracks]]
track_id = "btc-core"
symbol = "BTCUSDT"
lower_price = 90.0
upper_price = 110.0
long_exposure_units = 8.0
short_exposure_units = 4.0
notional_per_unit = 375.0
daily_loss_limit = 300.0
total_loss_limit = 600.0
shape_family = "concave"
"#,
    )
    .unwrap_err();

    assert!(format!("{error:#}").contains("concave"));
    assert!(format!("{error:#}").contains("inertial"));
}
```

在 `protocol/src/lib.rs` 增加：

```rust
#[test]
fn shape_family_serializes_new_behavior_names() {
    let payload = serde_json::to_string(&ShapeFamily::Responsive).unwrap();
    assert_eq!(payload, "\"responsive\"");
    assert_eq!(ShapeFamily::Inertial.to_string(), "inertial");
}

#[test]
fn shape_family_rejects_legacy_geometry_names() {
    assert!(serde_json::from_str::<ShapeFamily>("\"concave\"").is_err());
    assert!(serde_json::from_str::<ShapeFamily>("\"convex\"").is_err());
}
```

在 `core/src/strategy.rs` 的 `#[cfg(test)]` 模块新增或替换成下面这组测试：

```rust
fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 0.02,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn neutral_curve_is_symmetric_around_center_for_every_shape_family() {
    for shape_family in [
        ShapeFamily::Linear,
        ShapeFamily::Inertial,
        ShapeFamily::Responsive,
    ] {
        let config = TrackConfig {
            shape_family,
            ..neutral_config()
        };

        let lower_side = desired_exposure(95.0, &config).0;
        let upper_side = desired_exposure(105.0, &config).0;

        assert_close(lower_side, -upper_side);
    }
}

#[test]
fn biased_curve_shifts_center_by_capacity_difference() {
    let config = TrackConfig {
        long_exposure_units: 10.0,
        short_exposure_units: 6.0,
        ..neutral_config()
    };

    assert_close(desired_exposure(100.0, &config).0, 2.0);
    assert_close(desired_exposure(90.0, &config).0, 10.0);
    assert_close(desired_exposure(110.0, &config).0, -6.0);
}

#[test]
fn stronger_shape_family_curves_have_clear_inventory_separation_halfway_to_center() {
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

    assert_close(inertial.0, 6.35);
    assert_close(linear.0, 4.0);
    assert_close(responsive.0, 1.0);
    assert!(inertial.0 > linear.0);
    assert!(linear.0 > responsive.0);
}
```

在 `engine/src/manager.rs` 增加一条恢复边界测试：

```rust
#[test]
fn restore_track_state_rejects_shape_family_revision_mismatch() {
    let mut manager = test_manager_with_active_track();
    let snapshot = {
        let mut runtime = TrackRuntime::new(
            TrackId::new("btc-core"),
            test_instrument("BTCUSDT"),
            TrackConfig {
                shape_family: ShapeFamily::Inertial,
                ..test_config()
            },
            test_budget(),
            test_exchange_rules(),
            Utc.with_ymd_and_hms(2026, 3, 29, 9, 0, 0).unwrap(),
        );
        runtime.status = TrackStatus::Active;
        runtime.current_exposure = poise_core::types::Exposure(0.0);
        runtime.reference_price = Some(90.0);
        runtime.snapshot()
    };

    let error = manager.restore_track_state(&snapshot).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("snapshot restore revision mismatch")
    );
}
```

在 `application/src/track_definition.rs` 把使用 `ShapeFamily::Concave` 的测试准备改成 `ShapeFamily::Inertial`，先让编译和测试失败。

- [x] **Step 2: 运行测试确认失败**

运行：

- `cargo test -p poise-server config::tests::parses_new_shape_family_names -- --exact --nocapture`
- `cargo test -p poise-server config::tests::rejects_legacy_shape_family_names_with_migration_hint -- --exact --nocapture`
- `cargo test -p poise-protocol tests::shape_family_serializes_new_behavior_names -- --exact --nocapture`
- `cargo test -p poise-protocol tests::shape_family_rejects_legacy_geometry_names -- --exact --nocapture`
- `cargo test -p poise-core strategy::tests::neutral_curve_is_symmetric_around_center_for_every_shape_family -- --exact --nocapture`
- `cargo test -p poise-core strategy::tests::biased_curve_shifts_center_by_capacity_difference -- --exact --nocapture`
- `cargo test -p poise-core strategy::tests::stronger_shape_family_curves_have_clear_inventory_separation_halfway_to_center -- --exact --nocapture`
- `cargo test -p poise-engine manager::tests::restore_track_state_rejects_shape_family_revision_mismatch -- --exact --nocapture`

预期：

- 配置和协议边界失败，因为当前还只认识 `concave / convex`
- `core` 失败，因为当前实现还是按整段区间单向计算
- `engine` 失败，因为当前代码里还没有锁住 `shape_family` 的恢复边界测试

实际执行说明：

- 用 `poise-server`、`poise-protocol`、`poise-core`、`poise-engine` 的代表性失败用例确认了当前实现还不满足新接口和新公式
- 失败原因与预期一致，主要是缺少 `Inertial / Responsive` 枚举和对称公式

- [x] **Step 3: 做最小实现，在同一个 task 里一起改名字和数学行为**

在 `core/src/strategy.rs`：

- 把 `ShapeFamily` 改成 `Linear / Inertial / Responsive`
- 新增中点坐标 helper，并把 `desired_exposure` 重写为“中点对称基准曲线 + 容量偏移”
- 保持 `band_status` 和 `base_qty_per_unit` 不变
- 保持端点行为不变：下沿仍是 `+long_exposure_units`，上沿仍是 `-short_exposure_units`

实现公式：

```rust
fn signed_band_position(price: f64, config: &TrackConfig) -> f64 {
    let half_band = (config.upper_price - config.lower_price) / 2.0;
    ((price - config.band_center()) / half_band).clamp(-1.0, 1.0)
}

fn mirrored_shape_value(position: f64, shape_family: ShapeFamily) -> f64 {
    let magnitude = match shape_family {
        ShapeFamily::Linear => position.abs(),
        ShapeFamily::Inertial => position.abs().powf(1.0 / 3.0),
        ShapeFamily::Responsive => position.abs().powi(3),
    };

    if position >= 0.0 {
        -magnitude
    } else {
        magnitude
    }
}

pub fn desired_exposure(price: f64, config: &TrackConfig) -> Exposure {
    let position = signed_band_position(price, config);
    let span = (config.long_exposure_units + config.short_exposure_units) / 2.0;
    let bias = (config.long_exposure_units - config.short_exposure_units) / 2.0;

    Exposure(bias + span * mirrored_shape_value(position, config.shape_family))
}
```

同时把 `desired_exposure` 上方注释改成新的口径：

```rust
/// 使用围绕价格带中点对称的控仓曲线：
/// - Linear:      h(u) = -sign(u) * |u|
/// - Inertial:    h(u) = -sign(u) * |u|^(1/3)
/// - Responsive:  h(u) = -sign(u) * |u|^3
```

在 `application/src/track_definition.rs`、`protocol/src/lib.rs`、`server/src/projector.rs`、`server/src/config.rs` 和 `tui/src/main.rs`：

- 全部改成新枚举名
- `server/src/config.rs` 接受 `shape_family = "inertial" / "responsive"`
- 对旧值返回明确迁移提示：

```text
shape_family `concave` has been renamed to `inertial`
shape_family `convex` has been renamed to `responsive`
```

在 `engine/src/manager.rs`：

- 不新增兼容层
- 保留现有 `restore_revision` 机制
- 用测试明确锁住：`shape_family` 变化后旧 snapshot 不能恢复

要求：

- 不允许先提交“只改名字”的中间状态
- 本 task 完成前，不得产生一个名字已变但行为仍旧的 commit

- [x] **Step 4: 跑边界测试和编译回归**

运行：

- `cargo test -p poise-core strategy::tests:: -- --nocapture`
- `cargo test -p poise-server config::tests:: -- --nocapture`
- `cargo test -p poise-application track_definition::tests:: -- --nocapture`
- `cargo test -p poise-protocol tests:: -- --nocapture`
- `cargo test -p poise-engine manager::tests::restore_track_state_rejects_restore_revision_mismatch -- --exact --nocapture`
- `cargo test -p poise-engine manager::tests::restore_track_state_rejects_shape_family_revision_mismatch -- --exact --nocapture`
- `cargo test -p poise-tui --no-run`

预期：

- 新名字、对称公式和恢复边界相关测试全部 PASS
- `poise-tui` 能通过编译，说明联调夹具里的旧名字已经清干净

- [x] **Step 5: Commit**

```bash
git add core/src/strategy.rs application/src/track_definition.rs protocol/src/lib.rs server/src/projector.rs server/src/config.rs engine/src/manager.rs tui/src/main.rs
git commit -m "feat(strategy): rename and symmetrize shape families"
```

执行后在本 task 末尾回写 commit SHA。

Commit: `ff53dc3`

### Task 2: 更新文档，但不把当前定义复制到两份 spec

**Files:**
- Modify: `README.md`
- Modify: `docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md`
- Modify: `docs/superpowers/specs/assets/2026-04-10-shape-family-symmetry-examples.svg`
- Modify: `docs/superpowers/plans/2026-04-10-shape-family-symmetry.md`

- [ ] **Step 1: 更新 README，只保留当前行为语义**

在 `README.md` 的配置说明附近加入：

```md
- `shape_family` 当前支持 `linear`、`inertial`、`responsive`
- 三者都按“围绕价格带中点对称”的控仓曲线解释
- `inertial` 更恋边，从上下两侧往中间收仓都更慢
- `responsive` 更恋中，从上下两侧往中间收仓都更快
- `long_exposure_units` 和 `short_exposure_units` 只决定曲线整体上移或下移；`long > short` 表示偏多，`long < short` 表示偏空
```

不要在 README 里写 `p = 1 / 3` 或 `p = 3.0`。

如有迁移说明，只写一句：

```md
- 旧的 `concave / convex` 配置和值对应的持久化状态不再兼容，需要先清理后再启动
```

- [ ] **Step 2: 把旧策略设计文档改成历史说明**

在 `docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md` 的 shape family 章节前加一段说明：

```md
> 历史说明：本节保留为 2026-03-24 的探索记录。`shape_family` 当前定义见 [2026-04-10-shape-family-symmetry-design.md](./2026-04-10-shape-family-symmetry-design.md)。
```

要求：

- 保留旧文档作为历史记录
- 不把当前公式、当前命名和当前示例再复制一遍进去

- [ ] **Step 3: 更新示意图标签**

在 `docs/superpowers/specs/assets/2026-04-10-shape-family-symmetry-examples.svg`：

- 保持曲线走势不变
- 只维护当前名字和当前中文说明

- [ ] **Step 4: 用文本检查确认文档边界清楚**

运行：

- `rg -n "\\binertial\\b|\\bresponsive\\b" README.md docs/superpowers/specs/assets/2026-04-10-shape-family-symmetry-examples.svg`
- `rg -n "历史说明|2026-04-10-shape-family-symmetry-design.md" docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md`
- `cargo test --workspace --quiet`

预期：

- README 和示意图只呈现当前名字
- 旧 spec 明确标成历史记录，并指向新设计
- `cargo test --workspace --quiet` PASS

- [ ] **Step 5: Commit**

```bash
git add README.md docs/superpowers/specs/2026-03-24-grid-strategy-family-design.md docs/superpowers/specs/assets/2026-04-10-shape-family-symmetry-examples.svg docs/superpowers/plans/2026-04-10-shape-family-symmetry.md
git commit -m "docs(strategy): document symmetric shape families"
```

执行后在本 task 末尾回写 commit SHA。
