# OKX 币本位反向合约实现计划

## 状态

本计划基于 [okx-coin-margined-spec.md](okx-coin-margined-spec.md)。当前按任务清单逐项执行。

执行本 plan 时遵守项目约定：

- 每个 task 先补或调整验收测试，再实现。
- 每个 task 只做一个可验证闭环。
- 每个 task 验收通过后立即提交，并把 commit SHA 回写到本文任务清单。
- 如果实现中发现当前设计需要新增独立 service、多层 DTO、跨层临时状态，或需要改变已确认语义，先停止并确认，不继续硬改。

## 设计记录

主导复杂度是数量单位和资产单位的认知负担。设计上只新增一个 owner：`ExchangeRules` 数量语义。交易所 adapter 拥有交易所 metadata 和原始字段知识；core / engine 只看 `native quantity`、`settlement_asset` 和 `ExchangeRules` 的两个方法。

第一版不新增独立 `QuantityModel` service，不新增 `loss_limit_asset` 配置，不接 OKX `max-size`。展示字段不作为事实源；PNL asset 是历史事实，必须进入 `TrackPnlRecord` 持久化。

## Task 清单

| Task | 状态 | 验收 | Commit |
| --- | --- | --- | --- |
| 1. 扩展 `ExchangeRules` 数量语义 | completed | 已通过：`cargo test -p poise-core quantity`；四个 exchange mapper 测试；`poise-engine`、`poise-server`、`poise-application` no-run | `ccc3947a5aa259eb66f1e302f4e47c0078e00e83` |
| 2. engine 使用 native quantity 规划和风控 | completed | 已通过：`poise-core` track/strategy；`poise-engine` executor/manager/runtime/execution_plan；`poise-server` startup_bootstrap | `4a10b9a9ba29d446114ca0679b6e87d5b474f5b5` |
| 3. 持久化 PNL asset 并按同资产聚合 | completed | 已通过：`poise-storage` schema/pnl；`poise-engine` ledger/loss_guard/manager/reconciler；`poise-core` risk；`poise-application` runtime_lifecycle/mutation_executor；四个 exchange WS；`poise-server` startup_bootstrap | `4f3a797065b4e50622a7df21dd06871f8ef6ecec` |
| 4. OKX metadata、数量映射和 PNL asset | completed | 已通过：`cargo test -p poise-okx`；`cargo test -p poise-server runtime::startup_bootstrap::tests::` | `6f084aadba3046a3ca030daa5fae78b74a4a9930` |
| 5. OKX account capacity 本地估算 | completed | 已通过：`cargo test -p poise-okx`；`cargo test -p poise-server runtime::startup_bootstrap::tests::`；`cargo test -p poise-server runtime::guards::tests::`；`poise-server`、`poise-application`、`poise-storage` no-run；Binance/Bybit/Hyperliquid mapper/WS 相关测试 | `79518118e7a046970c966ae20d8bb682d65cfcf4` |
| 6. protocol / projector 单位展示 | completed | 已通过：`cargo test -p poise-protocol`；`cargo test -p poise-application read_model::tests::`；`cargo test -p poise-engine runtime::tests::`；`cargo test -p poise-server projector::tests::`、`http::tests::`、`websocket::tests::`、`runtime::diagnostics::tests::`；`pnpm exec tsc -b` | `b262be7d03e154c509a4f502e1bcdf488ca5a7db` |
| 7. 最终回归验收 | pending | 未执行 |  |

## Task 1. 扩展 `ExchangeRules` 数量语义

目标：让数量差异由现有 `ExchangeRules` 承担，避免 engine 到处判断 base quantity 和 inverse contract。

改动范围：

- `core/src/types.rs`
- 各交易所 mapper 中构造 `ExchangeRules` 的位置
- 测试 helper 中构造 `ExchangeRules` 的位置

实现要点：

- 增加 `QuantityKind`：
  - `base_asset`
  - `inverse_contract`
- 在 `ExchangeRules` 增加：
  - `quantity_kind`
  - `contract_notional`
  - `settlement_asset`
- `base_asset` 下 `contract_notional` 为空或等价无效值，方法不得依赖它。
- 增加两个方法：
  - `native_qty_per_exposure_unit(notional_per_unit, band_center)`
  - `notional_from_native_qty(native_qty, price)`
- 默认行为保持 U 本位现有语义：base asset 数量，`notional_from_native_qty = abs(qty) * price`。

验收测试：

- `base_asset`：`notional_per_unit=1000`、`band_center=100000` 得到 `0.01 BTC`。
- `inverse_contract`：`contract_notional=100`、`notional_per_unit=1000` 得到 `10 张`。
- `inverse_contract` 的 `notional_from_native_qty(-30, price)` 不随 price 变化，结果为 `3000`。
- 构造缺失 `contract_notional` 的 `inverse_contract` 时行为明确：返回错误或安全的零值，不能 panic。

建议验证命令：

```bash
cargo test -p poise-core quantity
```

设计停点：

- 如果为了新增字段需要大面积手写默认值，优先加小的构造 helper。
- 如果开始出现独立 quantity service 或多层包装，停止确认。

## Task 2. engine 使用 native quantity 规划和风控

目标：让 current exposure、target quantity、最小名义判断、max notional 判断都通过 `ExchangeRules` 数量方法。

改动范围：

- `core/src/strategy.rs`
- `core/src/track.rs`
- `engine/src/manager.rs`
- `engine/src/executor/planning.rs`
- `engine/src/executor/policy.rs`
- `engine/src/execution_plan.rs`
- `server/src/runtime/startup_bootstrap.rs`
- `server/src/runtime/mod.rs`

实现要点：

- 保留 `TrackConfig::base_qty_per_unit()` 作为兼容或测试辅助时，不再让新执行路径依赖它。
- 在 runtime / manager 层使用 `track.config()` + `track.exchange_rules()` 计算：
  - `native_qty_per_exposure_unit`
  - `current_exposure`
  - `target_native_qty`
  - 当前 position notional
- `is_meetable_minimum(price, quantity, rules)` 改为调用 `rules.notional_from_native_qty(quantity, price)`。
- `required_additional_notional` 不再只靠 `TrackDefinition` 和裸 position qty；需要有 `ExchangeRules` 参与，优先把行为放在 runtime seed / runtime wrapper，而不是把 `ExchangeRules` 塞进 `TrackDefinition`。

验收测试：

- U 本位现有 planning / policy / manager 行为不变。
- inverse：`ctVal=100`、`notional_per_unit=1000`、position `-30 张` 得到 exposure `-3`。
- inverse：price 从 `100000` 改到 `50000`，同一 position exposure 不变。
- inverse：最小名义判断不使用 `price * contracts`。

建议验证命令：

```bash
cargo test -p poise-core track::tests:: strategy::tests::
cargo test -p poise-engine executor:: planning:: policy:: manager::
cargo test -p poise-server runtime::startup_bootstrap::
```

设计停点：

- 如果发现 `TrackDefinition` 必须持有 `ExchangeRules` 才能继续工作，先停止确认。优先保持 `TrackDefinition` 是纯策略配置，数量语义由 runtime 持有的 rules 参与。

## Task 3. 持久化 PNL asset 并按同资产聚合

目标：`pnl_asset` 成为历史事实，daily loss 和展示不再靠 symbol 临时推断。

改动范围：

- `engine/src/ledger.rs`
- `engine/src/loss_guard.rs`
- `storage/src/schema.rs`
- `storage/src/sqlite.rs`
- 各 exchange WS / mapper 创建 `TrackPnlRecord` 的位置

实现要点：

- `TrackPnlRecord` 增加 `pnl_asset`。
- `TrackPnlStats` 增加 `pnl_asset`，第一版只聚合同一资产。
- `LossGuardSnapshot` 第一版不增加资产字段，保持 core risk 纯数字；`engine/src/loss_guard.rs` 在构建 snapshot 前校验 `pnl_stats.pnl_asset == ExchangeRules.settlement_asset`。
- SQLite `track_pnl_records` 增加 `pnl_asset` column。
- 新写入记录必须带 `pnl_asset`。
- 旧记录兼容策略：旧 DB 中 `pnl_asset IS NULL` 的记录只按 legacy quote asset 兼容读取；新记录不能为空。
- 聚合时如果同一 track 出现多个有效 `pnl_asset`，返回错误，不做跨资产净额。

验收测试：

- storage 插入和读取 `TrackPnlRecord.pnl_asset`。
- 同一 track 同一 asset 可聚合。
- 同一 track 多 asset 聚合失败或明确拒绝。
- loss guard 构建时拒绝 `pnl_stats.pnl_asset` 和 `settlement_asset` 不一致。
- loss guard 使用 BTC 数字和 BTC limit 时触发逻辑正确。
- U 本位 legacy 测试仍通过。

建议验证命令：

```bash
cargo test -p poise-engine ledger:: loss_guard::
cargo test -p poise-storage pnl
cargo test -p poise-core risk::
```

设计停点：

- 如果为了兼容旧 SQLite 需要在多个层到处推断 quote asset，停止确认。允许一次性 legacy 兼容，但不能让新事实继续靠 symbol 推导。

## Task 4. OKX metadata、数量映射和 PNL asset

目标：OKX adapter 解析 instrument metadata，并把 inverse 暴露为合约张数；OKX U 本位 SWAP 不把张数暴露到 core / engine。

改动范围：

- `exchanges/okx/src/rest/models.rs`
- `exchanges/okx/src/mapper.rs`
- `exchanges/okx/src/rest/client.rs`
- `exchanges/okx/src/ws/account.rs`
- `exchanges/okx/src/connected.rs`

实现要点：

- `InstrumentInfo` 解析：
  - `ctType`
  - `ctVal`
  - `ctValCcy`
  - `settleCcy`
- `BTC-USD-SWAP` 映射为：
  - `quantity_kind = inverse_contract`
  - `contract_notional = ctVal`
  - `settlement_asset = settleCcy`
  - `quantity_step = lotSz`
  - `min_qty = minSz`
- OKX U 本位 SWAP：
  - core / engine 仍看到 base asset 数量。
  - REST position / open orders / fills 如果原始是张数，adapter 内部用 `ctVal` 转成 base asset qty。
  - submit order 时，adapter 内部把 base asset qty 转回 OKX `sz`。
- OKX inverse：
  - submit / position / order / fill qty 保持张数。
- OKX trade PNL / fee 写入 `TrackPnlRecord.pnl_asset = settleCcy`。
- 如果 OKX 返回 `feeCcy` 且不等于 `settleCcy`，第一版 fail closed，不把不同资产净在一起。

验收测试：

- OKX inverse instrument fixture 映射出正确 rules。
- OKX U 本位 instrument fixture 映射后 engine-facing quantity 是 base asset。
- OKX inverse position `pos=-30` 映射为 `Position.qty=-30`。
- OKX inverse order `sz=30` / `accFillSz=10` 映射为张数。
- OKX inverse fillPnl / fee 生成 BTC `pnl_asset`。
- OKX U 本位 submit / position / open order 数量转换不回归。

建议验证命令：

```bash
cargo test -p poise-exchange-okx mapper:: ws:: rest::
```

设计停点：

- OKX REST / WS 当前 mapper 没有自然的 symbol rules lookup。若实现 U 本位内部转换需要引入超出 adapter 内部的小型 cache / lookup，先停止确认。
- 如果 OKX fixture 证明 `fillPnl`、`upl` 或 fee 资产不稳定，不继续接入 PNL，先确认资产规则。

## Task 5. OKX account capacity 本地估算

目标：第一版不调用 OKX `max-size`，使用 `available_by_asset[settlement_asset]`、mark price、leverage 和 `contract_notional` 本地估算账户容量。

改动范围：

- `engine/src/ports.rs`
- `exchanges/okx/src/rest/models.rs`
- `exchanges/okx/src/mapper.rs`
- `exchanges/okx/src/rest/client.rs`
- `server/src/runtime/guards.rs`
- `server/src/runtime/startup_bootstrap.rs`
- `server/src/effect_worker/execute.rs`

实现要点：

- Account summary 需要能表达按资产区分的 available，例如 `available_by_asset`。
- OKX balance mapper 从 balance details 填充 `available_by_asset`。
- OKX inverse capacity：

```text
estimated_max_contracts = available_btc * mark_price * leverage / contract_notional
estimated_max_notional = estimated_max_contracts * contract_notional
```

- U 本位 capacity 继续保持现有 USD-like notional 语义。
- mark price 来源优先使用实现时已有可靠字段；如果 OKX position / account 数据没有可靠 mark price，使用 runtime 已有 market price 需要先确认接口形状。
- 下单被交易所拒绝仍按现有 insufficient margin 流程处理并刷新账户状态。

验收测试：

- `available_by_asset["BTC"] = 0.5`、`mark_price=100000`、`leverage=2`、`contract_notional=100` 时，估算可开 `1000 张`，名义 `100000 USD`。
- mark price 降到 `50000` 时，可开张数降到 `500 张`。
- U 本位 capacity 现有测试不回归。

建议验证命令：

```bash
cargo test -p poise-exchange-okx account_capacity
cargo test -p poise-server runtime::guards:: runtime::startup_bootstrap::
```

设计停点：

- 如果 mark price 只能通过给 `AccountPort` 增加复杂 request DTO 才能取得，先停止确认。优先复用现有 runtime market price 或 OKX position 可用字段，但不要引入宽泛 capacity service。

## Task 6. protocol / projector 单位展示

目标：公开读模型不再把币本位数量和 PNL asset 展示错。

改动范围：

- `protocol/src/lib.rs`
- `server/src/projector.rs`
- 相关 websocket / http 投影测试

实现要点：

- `TrackPositionView` 增加 `quantity_unit`，值来自 `ExchangeRules.quantity_kind`：
  - `base_asset`
  - `contracts`
- `position.notional` 使用 `ExchangeRules::notional_from_native_qty`。
- `position.notional_asset` 由数量语义决定：inverse contract 显示 `USD`，U 本位保持现有 quote / USD-like 资产。
- `pnl.pnl_asset` 来自 `TrackPnlStats.pnl_asset` 或 `ExchangeRules.settlement_asset`，不能用 `instrument.quote_asset()`。
- 不在第一版增加 `estimated_base_quantity` 和 USD projection。

验收测试：

- BTC-USD-SWAP detail / list 投影：
  - `position.quantity_unit = "contracts"`
  - `position.notional = abs(contracts) * ctVal`
  - `position.notional_asset = "USD"`
  - `pnl.pnl_asset = "BTC"`
- U 本位现有 protocol fixtures 兼容。

建议验证命令：

```bash
cargo test -p poise-protocol position pnl_asset
cargo test -p poise-server projector::tests:: websocket::tests:: http::tests::
```

设计停点：

- 如果为了展示字段需要把 runtime 内部 state 暴露到 protocol，先停止确认。protocol 只能读 read model 已经拥有的规则和事实。

## Task 7. 最终回归验收

目标：确认 OKX inverse 支持形成闭环，且现有 U 本位行为不回归。

验收范围：

- OKX inverse metadata -> `ExchangeRules`。
- inverse position -> exposure。
- inverse planning -> submit quantity 张数。
- inverse open order recovery -> binding 张数。
- inverse PNL -> BTC asset 持久化 -> BTC daily loss。
- inverse capacity -> settlement asset 本地估算。
- Binance / Bybit / Hyperliquid 现有 U 本位行为不变。
- OKX U 本位如果 API 原始返回张数，adapter 内部转换后 engine 仍看到 base asset qty。

建议验证命令：

```bash
cargo test -p poise-core
cargo test -p poise-engine
cargo test -p poise-storage
cargo test -p poise-exchange-okx
cargo test -p poise-server exchange_startup::tests::
cargo test -p poise-server assembly::tests::
cargo test -p poise-server config::tests::
```

是否扩大到 workspace：

- 如果前面 task 触及 protocol、storage、engine、server 和 exchange 多 crate 的共享边界，最终可以跑：

```bash
cargo test --workspace
```

设计停点：

- 如果最终验收暴露出“U 本位按中点 base qty”和“币本位按固定 contract notional”在某个共享函数里被强行统一，停止确认，不用增加新 abstraction 掩盖语义差异。
