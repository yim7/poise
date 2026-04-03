# Track Activity / Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把用户活动流和内部诊断流拆开，保持主 `TrackDetailView.activity` 的稳定用户语义，同时通过独立 `/debug/tracks/:id/diagnostics` 接口暴露非稳定 diagnostics，并让 TUI 只在显式 debug 视角下加载和显示 diagnostics。

**Architecture:** 在服务端新增一个单一的 event presentation classifier，把底层 `recent_track_events` / `recent_effects` 先分区成中间 presentation 项，再分别由稳定 projector 和独立 debug query service 渲染成各自 DTO。classifier 不直接产出 protocol view 类型；主 HTTP / WebSocket 协议继续只暴露稳定详情，`/debug/...` diagnostics 由 debug query service 负责组装，HTTP 层只负责转发请求和错误映射；TUI 默认不受影响，仅在显式 debug 模式下按需请求 diagnostics。

**Tech Stack:** Rust workspace, `poise-protocol`, `poise-server`, `poise-tui`, Axum, Serde, Ratatui, Cargo tests

---

### Task 1: 定义 diagnostics 协议与主协议边界

**Files:**
- Modify: `protocol/src/lib.rs`
- Modify: `docs/protocol-contract.md`
- Test: `protocol/src/lib.rs`

- [x] **Step 1: 写失败测试，覆盖 diagnostics debug 视图序列化**

```rust
#[test]
fn deserializes_track_diagnostics_response() {
    let payload: TrackDiagnosticsView = serde_json::from_str(
        r#"{
            "items":[
                {
                    "ts":"2026-04-03T02:26:47Z",
                    "message":"target exposure -3.9534 -> -3.7500",
                    "level":"info"
                }
            ]
        }"#,
    )
    .unwrap();

    assert_eq!(payload.items.len(), 1);
    assert_eq!(payload.items[0].message, "target exposure -3.9534 -> -3.7500");
}
```

- [x] **Step 2: 运行定向测试，确认当前协议还不支持 diagnostics**

Run: `cargo test -p poise-protocol deserializes_track_diagnostics_response -- --exact`

Expected: 编译失败或测试失败，提示 `TrackDiagnosticsView` / `TrackDiagnosticItemView` 尚未定义。

- [x] **Step 3: 以最小实现补齐 protocol diagnostics 类型**

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackDiagnosticsView {
    pub items: Vec<TrackDiagnosticItemView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackDiagnosticItemView {
    pub ts: String,
    pub message: String,
    pub level: ActivityLevelView,
}
```

- [x] **Step 4: 更新协议文档，明确稳定主协议与 `/debug/...` 非稳定边界**

补充到 `docs/protocol-contract.md`：

```markdown
- `GET /tracks/:id` 继续返回稳定用户详情，不包含 diagnostics。
- `GET /debug/tracks/:id/diagnostics` 返回 `TrackDiagnosticsView`。
- `/debug/...` 下的 diagnostics 为 debug 专用、非稳定、best-effort 接口，不作为自动化或外部集成契约。
```

- [x] **Step 5: 运行测试，确认 protocol 与文档边界一致**

Run: `cargo test -p poise-protocol`

Expected: PASS

Result:

- `cargo test -p poise-protocol deserializes_track_diagnostics_response -- --exact` → 失败，错误为 `unresolved import super::TrackDiagnosticsView`
- `cargo test -p poise-protocol tests::deserializes_track_diagnostics_response -- --exact` → 通过，`1 passed`
- `cargo test -p poise-protocol` → 通过，`5 passed`

Commit: pending

### Task 2: 引入单一 classifier 与独立 debug query service，并把 `ExposureTargetChanged` 从 activity 挪到 diagnostics

**Files:**
- Create: `server/src/event_presentation.rs`
- Create: `server/src/debug_query_service.rs`
- Modify: `server/src/projector.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/http.rs`
- Test: `server/src/projector.rs`
- Test: `server/src/debug_query_service.rs`
- Test: `server/src/http.rs`

- [ ] **Step 1: 写失败测试，覆盖 `ExposureTargetChanged` 不再进入稳定 activity**

在 `server/src/projector.rs` 新增测试：

```rust
#[test]
fn project_activity_excludes_exposure_target_changed_events() {
    let source = source_with_failed_effect_and_recent_event();

    let activity = TrackProjector::new().project_activity(&source);

    assert_eq!(activity.len(), 1);
    assert_eq!(activity[0].message, "submit order rejected");
}
```

- [ ] **Step 2: 写失败测试，覆盖 diagnostics 返回 `ExposureTargetChanged`**

在 `server/src/http.rs` 新增集成测试：

```rust
#[tokio::test]
async fn get_track_diagnostics_returns_exposure_target_changed_events() {
    let response = router(app_state().await)
        .oneshot(
            Request::builder()
                .uri("/debug/tracks/btc-core/diagnostics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: TrackDiagnosticsView = serde_json::from_slice(&body).unwrap();

    assert!(payload
        .items
        .iter()
        .any(|item| item.message.contains("target exposure")));
}
```

- [ ] **Step 3: 运行定向测试，确认当前实现仍把该事件放进 activity 且没有 diagnostics 路由**

Run: `cargo test -p poise-server project_activity_excludes_exposure_target_changed_events -- --exact`

Expected: FAIL，当前 activity 仍包含 `target exposure ...`

Run: `cargo test -p poise-server get_track_diagnostics_returns_exposure_target_changed_events -- --exact`

Expected: FAIL，当前 `/debug/tracks/:id/diagnostics` 路由不存在。

- [ ] **Step 4: 新增单一 event presentation classifier**

在 `server/src/event_presentation.rs` 定义单一分类入口，例如：

```rust
pub enum PresentationAudience {
    Activity,
    Diagnostics,
}

pub struct PresentedEvent {
    pub ts: DateTime<Utc>,
    pub message: String,
    pub level: ActivityLevelView,
    pub audience: PresentationAudience,
}

pub fn classify_track_events(source: &TrackReadModel) -> Vec<PresentedEvent> {
    // 只在这里决定哪些事件属于 activity / diagnostics
}
```

最低要求：

- `DomainEvent::ExposureTargetChanged` 进入 diagnostics
- `recent_effects` 继续进入 activity
- 其他当前已面向用户的事件继续进入 activity

约束：

- classifier 只输出中间 presentation 语义项
- classifier 不依赖 `TrackDetailView`、`TrackDiagnosticsView`、`GridActivityItemView`、`TrackDiagnosticItemView`
- protocol DTO 的组装放在 projector / debug query service

- [ ] **Step 5: 让稳定 projector 只消费 classifier 的 activity 输出**

把 `TrackProjector::project_activity()` 改为调用 classifier，并只消费 `audience == Activity` 的中间项，再在本层渲染成 `GridActivityItemView`。

- [ ] **Step 6: 新增独立 debug query service，负责 diagnostics DTO 组装**

建议新建 `server/src/debug_query_service.rs`，接口例如：

```rust
pub struct TrackDebugQueryService {
    query_service: Arc<TrackQueryService>,
}

impl TrackDebugQueryService {
    pub async fn load_track_diagnostics(
        &self,
        track_id: &TrackId,
    ) -> Result<Option<TrackDiagnosticsView>> {
        // 读取 TrackReadModel
        // 调用 classifier
        // 只消费 audience == Diagnostics 的中间项
        // 在这里组装 TrackDiagnosticsView
    }
}
```

约束：

- diagnostics DTO 组装不放在 `http.rs`
- 如果未来新增 debug websocket / CLI / batch diagnostics，都复用这个 query 边界

- [ ] **Step 7: 写失败测试，覆盖 debug query service 正确投影 diagnostics**

在 `server/src/debug_query_service.rs` 新增测试，要求：

- `ExposureTargetChanged` 会出现在 diagnostics
- 非 diagnostics 事件不会误入
- 返回结果按时间排序

- [ ] **Step 8: 让 HTTP handler 只调用 debug query service**

在 `server/src/http.rs` / 路由装配中增加：

```rust
GET /debug/tracks/:id/diagnostics
```

handler 流程：

1. 调用 `TrackDebugQueryService::load_track_diagnostics`
2. 做 `200 / 404 / 500` 映射
3. 不在 handler 内组装 diagnostics items

- [ ] **Step 9: 运行定向测试，确认分类边界已经生效**

Run: `cargo test -p poise-server project_activity_excludes_exposure_target_changed_events -- --exact`

Expected: PASS

Run: `cargo test -p poise-server debug_query_service -- --nocapture`

Expected: PASS

Run: `cargo test -p poise-server get_track_diagnostics_returns_exposure_target_changed_events -- --exact`

Expected: PASS

- [ ] **Step 10: 跑服务端相关回归测试**

Run: `cargo test -p poise-server projector::tests::project_detail_includes_available_commands_and_activity -- --exact`

Expected: PASS，并更新断言为 activity 不再包含 `target exposure ...`

Run: `cargo test -p poise-server http::tests::get_grid_detail_returns_projected_detail -- --exact`

Expected: PASS，并补断言稳定详情里不含 diagnostics。

Run: `cargo test -p poise-server query_service::tests::load_detail_source_reads_snapshot_events_and_effects -- --exact`

Expected: PASS，证明这次没有把 diagnostics 组装重新塞回通用查询层。

### Task 3: 为 diagnostics 加入 TUI debug 视角的按需加载

**Files:**
- Modify: `tui/src/api_client.rs`
- Modify: `tui/src/protocol.rs`
- Modify: `tui/src/app.rs`
- Modify: `tui/src/input.rs`
- Modify: `tui/src/main.rs`
- Modify: `tui/src/views/instance.rs`
- Modify: `tui/tests/fixtures/track_detail_view.json`
- Create: `tui/tests/fixtures/track_diagnostics_view.json`
- Test: `tui/src/api_client.rs`
- Test: `tui/src/main.rs`
- Test: `tui/src/views/instance.rs`

- [ ] **Step 1: 写失败测试，覆盖 TUI 可解析 diagnostics 响应**

在 `tui/src/protocol.rs` 或 `tui/src/api_client.rs` 新增测试：

```rust
#[test]
fn deserializes_track_diagnostics_view_fixture() {
    let payload: TrackDiagnosticsView = serde_json::from_str(include_str!(
        "../tests/fixtures/track_diagnostics_view.json"
    ))
    .unwrap();

    assert_eq!(payload.items.len(), 1);
}
```

- [ ] **Step 2: 写失败测试，覆盖 instance 视图默认不显示 diagnostics，但 debug 模式显示**

在 `tui/src/views/instance.rs` 新增测试，要求：

- 默认渲染不出现 `Diagnostics`
- 打开 debug 视角后出现 `Diagnostics`
- diagnostics 面板包含 `target exposure ...`

- [ ] **Step 3: 写失败测试，覆盖切换 debug 后才触发 diagnostics HTTP 请求**

在 `tui/src/main.rs` 或 `tui/src/api_client.rs` 新增测试，要求：

- 启动时不请求 `/debug/tracks/:id/diagnostics`
- 触发 debug 动作后才请求该路径

- [ ] **Step 4: 运行定向测试，确认当前 TUI 还没有 diagnostics 概念**

Run: `cargo test -p poise-tui diagnostics -- --nocapture`

Expected: FAIL，提示缺少 diagnostics 类型、fixture 或 UI 逻辑。

- [ ] **Step 5: 在 TUI 协议与 API client 中补齐 diagnostics 请求**

最小接口：

```rust
pub async fn get_track_diagnostics(&self, track_id: &str) -> Result<TrackDiagnosticsView>;
```

路径固定为：

```text
/debug/tracks/:id/diagnostics
```

- [ ] **Step 6: 在 app 状态中加入显式 debug 视角与 diagnostics 缓存**

要求：

- 默认关闭 debug 视角
- diagnostics 为空时不影响正常详情展示
- 切换 track 时 debug 视角下重新按需拉取当前 track diagnostics

- [ ] **Step 7: 在 input / main / instance view 中接入显式 debug 开关**

建议最小交互：

- 新增一个调试快捷键，例如 `d`
- 第一次打开 debug 视角时加载 diagnostics
- `Instance` 页底部新增 `Diagnostics` 面板，仅在 debug 视角下渲染

- [ ] **Step 8: 运行定向测试，确认 TUI 只在 debug 视角显示 diagnostics**

Run: `cargo test -p poise-tui deserializes_track_diagnostics_view_fixture -- --exact`

Expected: PASS

Run: `cargo test -p poise-tui renders_grid_detail_execution_activity_and_commands -- --exact`

Expected: PASS，默认视图仍只显示 Activity。

Run: `cargo test -p poise-tui diagnostics -- --nocapture`

Expected: PASS，覆盖 debug 视角 diagnostics 加载与渲染。

### Task 4: 全链路验收与文档同步

**Files:**
- Modify: `docs/protocol-contract.md`
- Modify: `docs/superpowers/specs/2026-04-03-track-activity-diagnostics-split-design.md`
- Modify: `docs/superpowers/plans/2026-04-03-track-activity-diagnostics-split.md`

- [ ] **Step 1: 跑协议、服务端、TUI 相关验收测试**

Run: `cargo test -p poise-protocol`

Expected: PASS

Run: `cargo test -p poise-server projector::tests::project_activity_distinguishes_superseded_submit_from_success -- --exact`

Expected: PASS

Run: `cargo test -p poise-server get_track_diagnostics_returns_exposure_target_changed_events -- --exact`

Expected: PASS

Run: `cargo test -p poise-tui diagnostics -- --nocapture`

Expected: PASS

- [ ] **Step 2: 跑一次跨 crate 回归**

Run: `cargo test -p poise-server -p poise-tui -p poise-protocol`

Expected: PASS

- [ ] **Step 3: 同步文档与任务清单状态**

要求：

- `docs/protocol-contract.md` 记录 `/debug/tracks/:id/diagnostics` 的非稳定 debug 语义
- spec 与实际接口、TUI debug 行为一致
- 计划文档勾选已完成步骤，并记录实际验证命令结果
