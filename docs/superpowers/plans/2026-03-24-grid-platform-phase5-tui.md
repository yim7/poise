# 网格平台第五阶段实现计划：grid-tui

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现终端 UI 客户端，通过 HTTP/WS 连接 grid-server，提供运维监控和命令操作。

**Architecture:** grid-tui 是独立二进制，不依赖 grid-engine 或 grid-core。只通过 HTTP/WS 协议与 grid-server 通信。详见[架构设计 spec](../specs/2026-03-24-grid-platform-architecture-design.md)。

**Tech Stack:** Rust, ratatui (TUI 框架), reqwest (HTTP), tokio-tungstenite (WebSocket)

**前置依赖：** 第四阶段（grid-server）已完成。

---

## File Structure

### 新建文件

```
tui/
├── Cargo.toml
└── src/
    ├── main.rs         # 启动入口
    ├── app.rs          # 应用状态和生命周期
    ├── api_client.rs   # HTTP/WS 客户端（与 grid-server 通信）
    ├── protocol.rs     # 线协议类型（与 server 共享的 JSON schema）
    ├── views/
    │   ├── mod.rs
    │   ├── dashboard.rs    # 主面板：实例概览
    │   ├── instance.rs     # 单实例详情
    │   └── help.rs         # 帮助页
    ├── input.rs        # 键盘输入处理
    └── theme.rs        # 颜色和样式
```

### 修改文件

- `Cargo.toml`（workspace 根）：添加 `"tui"` 到 members

---

### Task 1: 初始化 grid-tui crate

**Files:**
- Modify: `Cargo.toml`
- Create: `tui/Cargo.toml`
- Create: `tui/src/main.rs`

- [ ] **Step 1: 添加 ratatui 到 workspace 依赖**

```toml
ratatui = "0.29"
crossterm = "0.28"
```

- [ ] **Step 2: 创建 tui/Cargo.toml**

依赖 ratatui、crossterm、reqwest、tokio-tungstenite、serde、tokio。

注意：不依赖 grid-core 或 grid-engine。

- [ ] **Step 3: 创建 main.rs 骨架**

能启动 TUI alternate screen 并正常退出。

- [ ] **Step 4: 验证编译和运行**

Run: `cargo run -p grid-tui`
Expected: 进入 alternate screen，按 q 退出

- [ ] **Step 5: 提交**

```bash
git add -A && git commit -m "feat: initialize grid-tui crate with basic terminal setup"
```

---

### Task 2: 线协议类型

**Files:**
- Create: `tui/src/protocol.rs`

- [ ] **Step 1: 定义客户端侧协议类型**

与 grid-server HTTP/WS 响应对应的 serde struct：
- `InstanceSummary`（来自 `GET /instances`）
- `InstanceSnapshot`（来自 `GET /instances/{id}/snapshot`）
- `CommandResponse`（来自 `POST /instances/{id}/commands`）
- `WsEvent`（来自 WebSocket 推送）

- [ ] **Step 2: 写反序列化测试**

用 JSON fixture 测试反序列化。

- [ ] **Step 3: 运行测试确认通过**

- [ ] **Step 4: 提交**

```bash
git add -A && git commit -m "feat(tui): add wire protocol types"
```

---

### Task 3: API 客户端

**Files:**
- Create: `tui/src/api_client.rs`

- [ ] **Step 1: 实现 HTTP 客户端**

```rust
pub struct ApiClient {
    base_url: String,
    http: reqwest::Client,
}

impl ApiClient {
    pub async fn list_instances(&self) -> Result<Vec<InstanceSummary>>;
    pub async fn get_snapshot(&self, id: &str) -> Result<InstanceSnapshot>;
    pub async fn submit_command(&self, id: &str, cmd: &str) -> Result<CommandResponse>;
}
```

- [ ] **Step 2: 实现 WebSocket 客户端**

```rust
pub async fn connect_ws(url: &str) -> Result<mpsc::Receiver<WsEvent>>;
```

- [ ] **Step 3: 验证编译通过**

- [ ] **Step 4: 提交**

```bash
git add -A && git commit -m "feat(tui): add HTTP/WS API client"
```

---

### Task 4: 应用状态和输入处理

**Files:**
- Create: `tui/src/app.rs`
- Create: `tui/src/input.rs`

- [ ] **Step 1: 实现 App struct**

```rust
pub struct App {
    pub instances: Vec<InstanceSummary>,
    pub current_instance: Option<InstanceSnapshot>,
    pub selected_index: usize,
    pub current_view: View,
    pub should_quit: bool,
}

pub enum View { Dashboard, Instance, Help }
```

- [ ] **Step 2: 实现输入处理**

- `q` — 退出
- `↑/↓` 或 `k/j` — 选择实例
- `Enter` — 进入实例详情
- `Esc` — 返回面板
- `?` — 帮助
- `[/]` — 切换实例

- [ ] **Step 3: 写输入处理测试**

- [ ] **Step 4: 提交**

```bash
git add -A && git commit -m "feat(tui): add app state and input handling"
```

---

### Task 5: 视图渲染

**Files:**
- Create: `tui/src/views/mod.rs`
- Create: `tui/src/views/dashboard.rs`
- Create: `tui/src/views/instance.rs`
- Create: `tui/src/views/help.rs`
- Create: `tui/src/theme.rs`

- [ ] **Step 1: 实现 Dashboard 视图**

显示所有实例的摘要表格：ID、Symbol、Status、Exposure、Last Price。

- [ ] **Step 2: 实现 Instance 详情视图**

显示单实例详情：配置、状态、目标占用、带内/带外、最近事件。

- [ ] **Step 3: 实现 Help 视图**

显示快捷键说明。

- [ ] **Step 4: 实现 Theme**

颜色和样式定义。

- [ ] **Step 5: 验证编译通过**

- [ ] **Step 6: 提交**

```bash
git add -A && git commit -m "feat(tui): add dashboard, instance and help views"
```

---

### Task 6: 集成主循环

**Files:**
- Modify: `tui/src/main.rs`

- [ ] **Step 1: 串联完整 TUI 主循环**

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let client = ApiClient::new(base_url);
    let instances = client.list_instances().await?;
    let mut app = App::new(instances);

    let mut terminal = setup_terminal()?;
    let ws_rx = connect_ws(&ws_url).await?;

    loop {
        terminal.draw(|f| render(&app, f))?;
        // poll input + ws events
        // handle input → update app state
        // handle ws event → update app state
        if app.should_quit { break; }
    }

    restore_terminal()?;
    Ok(())
}
```

- [ ] **Step 2: 端到端测试**

启动 server + tui，验证：
- TUI 显示实例列表
- 可以切换实例
- WebSocket 事件实时更新

- [ ] **Step 3: 提交**

```bash
git add -A && git commit -m "feat(tui): integrate full TUI main loop"
```

---

## 验收标准

1. `cargo build -p grid-tui` 编译成功
2. grid-tui 不依赖 grid-core 或 grid-engine
3. 启动后能通过 HTTP 拉取实例列表和快照
4. WebSocket 连接能接收并展示实时事件
5. 键盘操作流畅：切换实例、进入详情、退出
