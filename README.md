# 网格平台

这是一个用于承载网格交易服务端与运维终端客户端的 Rust workspace。

## 目录结构

- [`docs/`](docs/)
  - 技术方案
  - 路线图与近期计划
- [`service/`](service/)
  - Rust 服务端子项目
  - 核心控制面与未来引擎宿主
  - 通过 HTTP / WebSocket 服务 `tui` 与未来 Web UI
- [`tui/`](tui/)
  - Rust 终端客户端
  - 基于 Ratatui 的终端运维界面
  - reducer / effects / render / runtime 分层实现与测试

## 文档入口

- [`docs/technical-architecture.md`](docs/technical-architecture.md)
- [`docs/binance-integration.md`](docs/binance-integration.md)
- [`docs/protocol-contract.md`](docs/protocol-contract.md)
- [`docs/roadmap.md`](docs/roadmap.md)
- [`docs/plan.md`](docs/plan.md)
- [`TODO.md`](TODO.md)

## 验证方式

```bash
cargo test
```

## 说明

- 当前仓库由 `service` 和 `tui` 两个 crate 组成。
- `tui` 通过 HTTP + WebSocket 消费服务端能力。
- 协议约束以 Rust 类型定义和线协议语义文档为准。
- 后续如果增加 Web UI，它会作为新的独立客户端复用同一套控制面。
