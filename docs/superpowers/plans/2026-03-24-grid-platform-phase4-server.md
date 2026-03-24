# 网格平台第四阶段实现计划：grid-server

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现服务端入口，包含组件组装、HTTP API、WebSocket 事件推送和配置解析。

**Architecture:** 六边形架构的最外层。grid-server 把 grid-engine + 适配器组装在一起，通过 HTTP/WS 对外提供服务。详见[架构设计 spec](../specs/2026-03-24-grid-platform-architecture-design.md)。

**Tech Stack:** Rust, axum (HTTP), tokio-tungstenite (WebSocket), toml (配置)

**前置依赖：** 第一到第三阶段全部完成。

---

## File Structure

### 新建文件

```
server/
├── Cargo.toml
└── src/
    ├── main.rs         # 启动入口
    ├── config.rs       # 配置文件解析
    ├── assembly.rs     # 组件组装（依赖注入）
    ├── http.rs         # HTTP 路由和 handler
    └── websocket.rs    # WebSocket 事件推送
```

### 修改文件

- `Cargo.toml`（workspace 根）：添加 `"server"` 到 members

---

### Task 1: 初始化 grid-server crate

**Files:**
- Modify: `Cargo.toml`
- Create: `server/Cargo.toml`
- Create: `server/src/main.rs`

- [x] **Step 1: 添加 axum 到 workspace 依赖**

```toml
axum = "0.7"
tower-http = { version = "0.5", features = ["cors"] }
toml_edit = "0.22"
```

在 `[workspace].members` 中添加 `"server"`。

- [x] **Step 2: 创建 server/Cargo.toml**

依赖 grid-engine、grid-core、grid-binance、grid-storage 以及 axum、tokio 等。

- [x] **Step 3: 创建 main.rs 骨架**

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("grid-server starting");
    Ok(())
}
```

- [x] **Step 4: 验证编译和运行**

Run: `cargo run -p grid-server`
Expected: 打印 "grid-server starting" 后正常退出

- [x] **Step 5: 提交**

```bash
git add -A && git commit -m "feat: initialize grid-server crate"
```

---

### Task 2: 配置解析

**Files:**
- Create: `server/src/config.rs`

- [x] **Step 1: 写测试**

测试 TOML 配置文件解析：
- 解析 environment、bind_address
- 解析 `[[instances]]` 列表（symbol、price range、capacity）
- 解析交易所凭证（api_key、api_secret，可选）

- [x] **Step 2: 运行测试确认失败**

- [x] **Step 3: 实现 Config struct 和解析逻辑**

```rust
pub struct Config {
    pub environment: String,
    pub bind_address: String,
    pub instances: Vec<InstanceConfig>,
    pub exchange: ExchangeConfig,
}

pub struct InstanceConfig {
    pub symbol: String,
    pub lower_price: f64,
    pub upper_price: f64,
    pub long_capacity: f64,
    pub short_capacity: f64,
    pub capacity_notional: f64,
}

pub struct ExchangeConfig {
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub rest_base_url: Option<String>,
    pub ws_base_url: Option<String>,
}

pub fn load_config(path: &str) -> Result<Config>;
```

- [x] **Step 4: 运行测试确认通过**

- [x] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(server): add TOML config parsing"
```

---

### Task 3: 组件组装

**Files:**
- Create: `server/src/assembly.rs`

- [x] **Step 1: 实现组装逻辑**

```rust
pub struct Platform {
    pub manager: InstanceManager,
}

pub async fn assemble(config: &Config) -> Result<Platform> {
    let exchange = BinanceAdapter::new(&config.exchange)?;
    let persistence = SqliteStorage::new(&db_path)?;
    let clock = SystemClock::new();

    let mut manager = InstanceManager::new(
        Arc::new(exchange),
        Arc::new(persistence),
        Arc::new(clock),
    );

    for instance_config in &config.instances {
        manager.add_instance(...)?;
    }

    Ok(Platform { manager })
}
```

- [x] **Step 2: 验证编译通过**

- [x] **Step 3: 提交**

```bash
git add -A && git commit -m "feat(server): add component assembly"
```

---

### Task 4: HTTP 路由

**Files:**
- Create: `server/src/http.rs`

- [x] **Step 1: 写测试**

用 axum::test 测试：
- `GET /instances` 返回实例列表
- `GET /instances/{id}/snapshot` 返回实例快照
- `POST /instances/{id}/commands` 接受命令
- 不存在的实例返回 404

- [x] **Step 2: 运行测试确认失败**

- [x] **Step 3: 实现 HTTP handler**

```rust
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/instances", get(list_instances))
        .route("/instances/{id}/snapshot", get(get_snapshot))
        .route("/instances/{id}/commands", post(submit_command))
        .with_state(state)
}
```

- [x] **Step 4: 运行测试确认通过**

- [x] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(server): add HTTP routes - instances, snapshots, commands"
```

---

### Task 5: WebSocket 事件推送

**Files:**
- Create: `server/src/websocket.rs`

- [x] **Step 1: 实现 WebSocket handler**

- 接受 WS 连接
- 订阅 engine 的 DomainEvent 广播
- 将事件序列化为 JSON 推送给客户端
- 支持多客户端并发连接

- [x] **Step 2: 验证编译通过**

- [x] **Step 3: 提交**

```bash
git add -A && git commit -m "feat(server): add WebSocket event broadcasting"
```

---

### Task 6: 集成启动流程

**Files:**
- Modify: `server/src/main.rs`

- [x] **Step 1: 串联完整启动流程**

```rust
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let config = config::load_config(&args.config)?;
    let platform = assembly::assemble(&config).await?;
    let app = http::router(platform.into_state());
    let listener = tokio::net::TcpListener::bind(&config.bind_address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

- [x] **Step 2: 端到端测试**

启动 server，用 curl 验证：
- `GET /instances` 返回配置的实例列表
- `GET /instances/{id}/snapshot` 返回快照

- [x] **Step 3: 提交**

```bash
git add -A && git commit -m "feat(server): integrate full startup flow"
```

---

## 验收标准

1. `cargo test -p grid-server` 全部通过
2. `cargo run -p grid-server -- --config configs/test.toml` 能启动并响应 HTTP 请求
3. WebSocket 连接能接收实时事件
4. 组件组装只在 `assembly.rs` 一个文件里完成
5. engine/core 不依赖 axum 或任何 HTTP 框架
