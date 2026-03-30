# 网格平台第二阶段实现计划：poise-storage

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实现 SQLite 持久化适配器，完成 `PersistencePort` trait 的具体实现。

**Architecture:** 六边形架构适配器层。poise-storage 实现 poise-engine 中定义的 `PersistencePort` trait。详见[架构设计 spec](../specs/2026-03-24-grid-platform-architecture-design.md)。

**Tech Stack:** Rust, rusqlite, serde_json

**前置依赖：** 第一阶段（poise-core + poise-engine）已完成。

---

## File Structure

### 新建文件

```
storage/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── sqlite.rs       # PersistencePort 实现
    └── schema.rs       # 表结构与迁移
```

### 修改文件

- `Cargo.toml`（workspace 根）：添加 `"storage"` 到 members

---

### Task 1: 初始化 poise-storage crate

**Files:**
- Modify: `Cargo.toml`
- Create: `storage/Cargo.toml`
- Create: `storage/src/lib.rs`

- [x] **Step 1: 添加 rusqlite 到 workspace 依赖**

在 `Cargo.toml` 的 `[workspace.dependencies]` 中添加：

```toml
rusqlite = { version = "0.31", features = ["bundled"] }
serde_json = "1"
```

在 `[workspace].members` 中添加 `"storage"`。

- [x] **Step 2: 创建 storage/Cargo.toml**

```toml
[package]
name = "poise-storage"
version.workspace = true
edition.workspace = true

[dependencies]
poise-engine = { path = "../engine" }
poise-core = { path = "../core" }
rusqlite.workspace = true
serde.workspace = true
serde_json.workspace = true
anyhow.workspace = true
async-trait.workspace = true
tokio.workspace = true
chrono.workspace = true
```

- [x] **Step 3: 创建占位 lib.rs**

```rust
pub mod schema;
pub mod sqlite;
```

- [x] **Step 4: 验证编译**

Run: `cargo check -p poise-storage`
Expected: 编译成功

- [x] **Step 5: 提交**

```bash
git add -A && git commit -m "feat: initialize poise-storage crate"
```

---

### Task 2: 数据库 Schema

**Files:**
- Create: `storage/src/schema.rs`

- [x] **Step 1: 写测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    #[test]
    fn initialize_creates_tables() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();

        // 验证 instance_snapshots 表存在
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='instance_snapshots'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn initialize_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        initialize(&conn).unwrap();
        initialize(&conn).unwrap(); // 第二次不应失败
    }
}
```

- [x] **Step 2: 运行测试确认失败**

Run: `cargo test -p poise-storage -- schema`
Expected: FAIL

- [x] **Step 3: 实现 schema**

```rust
use anyhow::Result;
use rusqlite::Connection;

pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS instance_snapshots (
            id TEXT PRIMARY KEY,
            symbol TEXT NOT NULL,
            config_json TEXT NOT NULL,
            status TEXT NOT NULL,
            current_exposure REAL NOT NULL,
            last_price REAL,
            updated_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS domain_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            instance_id TEXT NOT NULL,
            event_json TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_events_instance
            ON domain_events(instance_id, created_at);",
    )?;
    Ok(())
}
```

- [x] **Step 4: 运行测试确认通过**

Run: `cargo test -p poise-storage -- schema`
Expected: PASS

- [x] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(storage): add database schema with instance_snapshots and domain_events"
```

---

### Task 3: SQLite PersistencePort 实现

**Files:**
- Create: `storage/src/sqlite.rs`

- [x] **Step 1: 写测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use grid_core::strategy::*;
    use grid_core::types::Exposure;
    use grid_engine::instance::InstanceStatus;
    use grid_engine::ports::InstanceSnapshot;

    fn test_snapshot() -> InstanceSnapshot {
        InstanceSnapshot {
            id: "test-1".into(),
            symbol: "BTCUSDT".into(),
            config: GridConfig {
                lower_price: 90.0,
                upper_price: 110.0,
                long_capacity: 8.0,
                short_capacity: 8.0,
                capacity_notional: 375.0,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: OutOfBandPolicy::Freeze,
            },
            status: InstanceStatus::Active,
            current_exposure: Exposure(4.0),
            last_price: Some(95.0),
        }
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let storage = SqliteStorage::in_memory().unwrap();
        let snapshot = test_snapshot();

        storage.save_instance_state("test-1", &snapshot).await.unwrap();
        let loaded = storage.load_instance_state("test-1").await.unwrap();

        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, "test-1");
        assert_eq!(loaded.symbol, "BTCUSDT");
        assert!((loaded.current_exposure.0 - 4.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let storage = SqliteStorage::in_memory().unwrap();
        let loaded = storage.load_instance_state("nonexistent").await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn save_overwrites_existing() {
        let storage = SqliteStorage::in_memory().unwrap();
        let mut snapshot = test_snapshot();

        storage.save_instance_state("test-1", &snapshot).await.unwrap();

        snapshot.current_exposure = Exposure(6.0);
        storage.save_instance_state("test-1", &snapshot).await.unwrap();

        let loaded = storage.load_instance_state("test-1").await.unwrap().unwrap();
        assert!((loaded.current_exposure.0 - 6.0).abs() < f64::EPSILON);
    }
}
```

- [x] **Step 2: 运行测试确认失败**

Run: `cargo test -p poise-storage -- sqlite`
Expected: FAIL

- [x] **Step 3: 实现 SqliteStorage**

实现 `SqliteStorage` struct 和 `PersistencePort` trait：
- `new(path)` 打开文件数据库并初始化 schema
- `in_memory()` 打开内存数据库用于测试
- `save_instance_state` 用 `INSERT OR REPLACE` 写入
- `load_instance_state` 用 `SELECT` 读取并反序列化

- [x] **Step 4: 运行测试确认通过**

Run: `cargo test -p poise-storage`
Expected: 全部 PASS

- [x] **Step 5: 提交**

```bash
git add -A && git commit -m "feat(storage): implement SqliteStorage with PersistencePort trait"
```

---

## 验收标准

1. `cargo test -p poise-storage` 全部通过
2. `SqliteStorage` 实现 `PersistencePort` trait
3. save → load roundtrip 数据完整
4. 支持 in-memory 模式用于测试
