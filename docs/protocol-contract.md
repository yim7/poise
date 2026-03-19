# 协议契约与兼容策略

本文档定义 `service` 与 `tui` 当前共享的 HTTP / WebSocket 协议约束，作为 Rust 协议类型定义之外的线协议语义补充。

## 1. 版本原则

- 当前协议版本固定为 `v1alpha1`。
- HTTP 成功响应、HTTP 错误响应、WebSocket 事件都必须带 `version`。
- 新增字段只能追加，不能删除或重命名既有字段，不能改变既有字段语义。
- 客户端必须忽略自己不认识的附加字段，服务端必须继续接受旧客户端仍会发送的既有字段。

## 2. HTTP envelope

### 2.1 成功响应

所有 HTTP `2xx` 响应统一使用以下结构：

```json
{
  "version": "v1alpha1",
  "status": "ok",
  "data": {}
}
```

约定：

- `data` 承载真实业务 payload。
- `data` 可以是对象，也可以是数组。
- 业务 payload 内已有的时间字段保持原语义，不因 envelope 改名。

### 2.2 错误响应

所有 HTTP 非 `2xx` 响应统一使用以下结构：

```json
{
  "version": "v1alpha1",
  "status": "error",
  "error": {
    "code": "validation_error",
    "message": "Request validation failed.",
    "details": []
  }
}
```

约定：

- `code` 是稳定的机器可读错误码。
- `message` 是面向操作者的简短说明。
- `details` 用于附带校验错误、下游错误或调试信息；调用方必须允许其为空或缺省。

## 3. WebSocket envelope

所有服务端事件统一使用以下结构：

```json
{
  "version": "v1alpha1",
  "event_id": "evt_xxx",
  "type": "runtime_snapshot",
  "emitted_at": "2025-01-01T00:00:00Z",
  "sequence": 12,
  "payload": {}
}
```

字段说明：

- `event_id`: 单条事件的唯一标识，用于日志关联、排障和未来去重。
- `type`: 事件类型。
- `emitted_at`: 服务端发出该事件的统一时间戳。所有事件都必须带，不能只依赖 payload 内部零散时间字段。
- `sequence`: 可选单调递增序号，为未来断线恢复和一致性校验预留；当前客户端必须允许其缺省。
- `payload`: 具体事件内容。

当前共享事件类型至少包括：

- `runtime_snapshot`
- `command_ack`
- `risk_alert`
- `connection_changed`

`connection_changed` 当前额外约定：

- `http_available` 表示 `service` 对 Binance REST 元数据同步是否健康。
- `ws_connected` 表示 `service` 对 Binance 市场 WebSocket 是否连通。
- `user_stream_connected` 为可选字段；未配置用户流时允许为 `null`。
- 客户端自身到 `service` 的传输连接状态不写入该 payload，应作为本地 transport 状态单独维护。

`runtime_snapshot.execution` 当前额外约定：

- `pending_commands`: 仍表示未终态命令。
- `last_command_ack`: 保留最近一次 ack 的 `command_id`，用于兼容旧客户端。
- `last_command_ack_event`: 提供最近一次完整 ack 内容，供重连恢复直接重建命令结果。
- `recent_commands`: 提供最近的命令终态记录，供客户端在断线后重建命令时间线。

## 4. 启动与重连语义

客户端必须遵循以下顺序：

1. 启动后先通过 HTTP 拉取 `runtime snapshot`。
2. 用该 `snapshot` 覆盖本地状态，不能依赖本地缓存补差。
3. 再建立 WebSocket 连接接收增量事件。
4. 一旦 WebSocket 断开并重连成功，必须重新拉取一次 HTTP `snapshot`。
5. 新拉到的 `snapshot` 必须覆盖本地状态，然后再继续消费后续增量事件。

这样设计的原因是：

- `snapshot` 是状态重建的权威来源。
- 客户端不负责回放历史日志补状态。
- `command_ack` 只负责命令闭环，不负责替代完整状态恢复。
- 断线期间完成的命令结果必须能仅靠 `snapshot.execution.last_command_ack_event` 和
  `snapshot.execution.recent_commands` 恢复出来。

## 5. 兼容演进规则

- 可以新增事件类型，但不能改变既有事件 `type` 的含义。
- 可以给 envelope 或 payload 追加字段，但不能要求旧客户端必须立刻理解新字段。
- 可以把 `sequence` 从缺省演进为稳定输出，但不能让旧客户端因为缺少 `sequence` 语义而无法工作。
- 若未来需要破坏性变更，必须提升 `version`，并保留旧版本一段兼容窗口。

## 6. 变更流程

每次协议变更都必须同时更新以下内容：

1. `service` 侧协议模型与接口测试。
2. `tui` 侧协议模型与协议测试。
3. 相关 Rust 协议类型定义与序列化测试。
4. 本文档或等价说明文档。
