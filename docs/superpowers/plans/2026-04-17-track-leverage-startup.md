# Track Leverage Startup Implementation Plan

> 执行本计划时，按仓库规则逐个 task 验收。每个 task 验收通过后，必须立即 `git add`、`git commit`，并把 commit SHA 回写到对应 task 下，再进入下一个 task。

**Goal:** 为每个 `track` 增加默认 `10x` 的 `leverage` 配置，并在服务启动时通过 server 自己的 symbol 启动控制边界设置交易所杠杆；任一 `track` 设置失败时，服务直接拒绝启动。

**Architecture:** `leverage` 字段只留在 `server` 配置和 server-owned 的 startup-only 杠杆索引中，不进入 `core::strategy::TrackConfig`、`ConfiguredTrackDefinition` 或 `TrackPreparedDefinition`。server 从配置构造一个简单的 `track_id -> leverage` 索引，然后在 `assembly` 中把 prepared track 的 `instrument` 和该索引里的杠杆组合起来，交给 `SymbolLeverageSetter` 执行设置。

**Scope Boundary:** 本轮只实现 symbol 级杠杆设置。未来如果有 `margin mode`、`position mode`，不能先假设它们跟 `leverage` 拥有同一种作用域模型；不同交易所可能是 account 级、symbol 级，或者混合层级。到那时应新增独立的 venue-aware bootstrap 边界，不塞进当前的 leverage setter。

**Venue Semantics:** server 对所有交易所统一执行“先准备 symbol 启动状态，再加载 metadata / capacity”的顺序；但这个顺序的理由是 venue-specific。Binance 需要它来保证容量快照反映目标杠杆，Bybit 当前则主要是为了让启动过程保持确定性，不改变现有容量语义。

---

## Task 1: 配置与 server-owned startup-only 杠杆索引补齐 leverage

**Files**

- Modify: `server/src/config.rs`
- Add or Modify: `server/src/exchange_startup.rs`

**Implementation**

- 在 `TrackFileDefinition` 上新增 `leverage: Option<u32>`
- 保持 `ConfiguredTrackInput`、`ConfiguredTrackDefinition`、`TrackPreparedDefinition` 不变
- 在 `server/src/exchange_startup.rs` 新增构造 startup-only 杠杆索引的 helper
- 该索引负责：
  - 缺省值展开为 `10`
  - `leverage == 0` 时拒绝配置
  - 按 `track_id` 保存 startup-only 杠杆
- `parse_config` 在 server 边界调用 startup projection 校验，确保错误尽早暴露

**Tests first**

- `server/src/config.rs`
  - `parses_explicit_track_leverage`
  - `rejects_zero_leverage_at_config_boundary`
  - 现有 `track_file_definition_maps_mechanically_to_configured_track_input` 保持不感知 `leverage`
- `server/src/exchange_startup.rs`
  - `track_leverage_index_defaults_to_ten`
  - `track_leverage_index_preserves_explicit_leverage`
  - `track_leverage_index_stores_only_startup_fields`

**Acceptance**

- `cargo test -p poise-server parses_explicit_track_leverage -- --exact --nocapture`
- `cargo test -p poise-server rejects_zero_leverage_at_config_boundary -- --exact --nocapture`
- `cargo test -p poise-server track_file_definition_maps_mechanically_to_configured_track_input -- --exact --nocapture`
- `cargo test -p poise-server track_leverage_index_defaults_to_ten -- --exact --nocapture`
- `cargo test -p poise-server track_leverage_index_preserves_explicit_leverage -- --exact --nocapture`
- `cargo test -p poise-server track_leverage_index_stores_only_startup_fields -- --exact --nocapture`

**Commit**

- Message: `feat(config): add track leverage startup index`
- Commit SHA: `c9bdaa1`

---

## Task 2: 交易所 crate 暴露最小 symbol leverage helper

**Files**

- Modify: `exchanges/binance/src/lib.rs`
- Modify: `exchanges/binance/src/rest/client.rs`
- Modify: `exchanges/binance/src/rest/models.rs`
- Add or Modify: `exchanges/binance/src/startup_control.rs`
- Modify: `exchanges/bybit/src/lib.rs`
- Modify: `exchanges/bybit/src/rest/client.rs`
- Modify: `exchanges/bybit/src/rest/models.rs`
- Add or Modify: `exchanges/bybit/src/startup_control.rs`

**Implementation**

- Binance crate 暴露公开的窄 helper，例如 `SymbolLeverageControl`
- Bybit crate 暴露公开的窄 helper，例如 `SymbolLeverageControl`
- helper 只负责最小动作：`set_leverage(symbol, leverage)`
- REST client 补齐对应交易所的设置杠杆接口
- 保留交易所原始错误文本
- 不修改 `engine/src/ports.rs`
- 不给 `Connected` 新增 `leverage()` getter

**Tests first**

- `exchanges/binance/src/rest/client.rs`
  - `set_leverage_posts_symbol_and_leverage`
- `exchanges/binance/src/startup_control.rs`
  - helper 会把 `symbol` 和目标杠杆正确转给 REST client
- `exchanges/bybit/src/rest/client.rs`
  - `set_leverage_uses_linear_position_body`
- `exchanges/bybit/src/startup_control.rs`
  - helper 会把 `symbol` 和目标杠杆正确转给 REST client

**Acceptance**

- `cargo test -p poise-binance set_leverage_posts_symbol_and_leverage -- --exact --nocapture`
- `cargo test -p poise-binance startup_control -- --nocapture`
- `cargo test -p poise-bybit set_leverage_uses_linear_position_body -- --exact --nocapture`
- `cargo test -p poise-bybit startup_control -- --nocapture`

**Commit**

- Message: `feat(exchange): add symbol startup leverage helpers`
- Commit SHA: `5f1ab68`

---

## Task 3: server 杠杆设置能力接入装配

**Files**

- Modify: `server/src/exchange_startup.rs`
- Modify: `server/src/assembly.rs`
- Modify: `server/src/config.rs` if startup registry access helpers are needed
- Modify: `server/src/main.rs` only if startup registry construction cannot stay inside existing server flow

**Implementation**

- 在 `server/src/exchange_startup.rs` 新增 `SymbolLeverageSetter`
- server 根据 `ExchangeConfig` 构建对应交易所 helper，并包成 `SymbolLeverageSetter`
- `assembly` 从 `config` 或 server helper 构造 `track_id -> leverage` 索引
- `assembly` 在每个 `track` 上先从 prepared track 读取 `instrument`，再从索引读取 `leverage`，然后调用 `set_leverage(instrument, leverage)`
- 之后再读取 `exchange info` 和 `account_capacity_snapshot`
- 错误文案带上 `track_id`、`symbol`、目标杠杆和交易所错误
- 保持现有 `Exchange` 运行时 port 结构稳定；如果必须改 `server/src/exchange.rs`，也只做最小改动，不新增通用 getter

**Tests first**

- `server/src/assembly.rs`
  - 启动时会先读取杠杆索引，再读取 metadata / capacity
  - `set_leverage` 失败时，服务启动失败
  - 错误文案包含 `track_id`、`symbol`、目标杠杆
- `server/src/exchange_startup.rs`
  - Binance / Bybit 分支会选择正确 helper
- 保留现有 runtime 测试，确认 `Exchange` 运行时装配没有因为这次改动继续变成更大的 pass-through 容器

**Acceptance**

- `cargo test -p poise-server assemble -- --nocapture`
- `cargo test -p poise-server exchange_startup -- --nocapture`

**Commit**

- Message: `feat(server): set track leverage during assembly`
- Commit SHA: `b023147`

---

## Task 4: 文档与全量验收

**Files**

- Modify: `README.md`
- Modify: `server/src/config.rs`
- Modify: `docs/superpowers/specs/2026-04-17-track-leverage-startup-design.md` if implementation forces minor wording adjustment
- Modify: `docs/superpowers/plans/2026-04-17-track-leverage-startup.md` 回写已执行的 commit SHA

**Implementation**

- 配置示例补上 `leverage = 10`
- 如果实现结果与 spec 有小偏差，先修正文档再结束
- 回写每个 task 的 commit SHA

**Acceptance**

- `cargo test -p poise-application`
- `cargo test -p poise-binance`
- `cargo test -p poise-bybit`
- `cargo test -p poise-server`

**Commit**

- Message: `docs: document track startup leverage behavior`
- Commit SHA: `TODO`

---

## Guardrails

- `engine/src/ports.rs` 本轮不新增杠杆相关共享 port
- `ConfiguredTrackDefinition` / `TrackPreparedDefinition` 本轮不新增 `leverage`
- `Connected` / `Exchange` 本轮不新增 `leverage()` 这类 pass-through getter
- `AccountCapacitySnapshot` 继续保持 venue-defined 语义
- `margin mode` / `position mode` 不是 `leverage`；如果后续支持，应先表达各交易所自己的作用域模型，再决定是否抽公共边界，不能直接塞进这次的 leverage setter
