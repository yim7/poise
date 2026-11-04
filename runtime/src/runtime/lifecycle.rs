//! 策略的生命周期：草稿、预览、确认启动、运行中修改参数（第 2.4 节）。

use chrono::{DateTime, Duration, Utc};
use poise_strategy::Strategy;
use poise_strategy::config::StrategyConfig;
use poise_strategy::instrument::{Fees, InstrumentSpec};
use poise_strategy::side::Direction;
use poise_strategy::state::StrategyState;
use poise_strategy::stress::{AccountBasis, Liquidation, SideStress, Suggestion};
use poise_strategy::units::{Notional, Position, Price, Quantity, Rate, SCALE};
use sha3::{Digest, Sha3_256};

use crate::exchange::ExchangeError;
use crate::settings::AccountSettings;
use crate::store::{
    DraftId, StoreError, StoredConfigVersion, StoredDraft, StoredStrategy, StrategyId,
    StrategyStatus, ViewSnapshot,
};

use super::{DraftPreview, Runtime, RuntimeError, Validation, fresh, percent};

impl Runtime {
    /// 新建草稿：只保存参数，不下任何单（第 2.4 节）。
    ///
    /// 参数这时不校验：合约规则和费率要问交易所，用户还没点预览。校验发生在预览和
    /// 确认启动时，用的是当时读到的规则。
    pub fn create_draft(&self, config: StrategyConfig) -> Result<DraftId, RuntimeError> {
        let now = self.clock.now();
        let draft_id = DraftId::new();
        self.store.insert_draft(&StoredDraft {
            draft_id: draft_id.clone(),
            config,
            created_at: now,
            updated_at: now,
        })?;
        tracing::info!(draft = %draft_id, "draft created");
        Ok(draft_id)
    }

    /// 保存草稿改过的参数，记一个新的参数版本，返回版本号（B42）。
    pub fn save_draft(
        &self,
        draft_id: &DraftId,
        config: StrategyConfig,
    ) -> Result<i64, RuntimeError> {
        let version = self
            .store
            .save_draft_config(draft_id, &config, self.clock.now())?;
        tracing::info!(draft = %draft_id, version, "draft saved");
        Ok(version)
    }

    pub fn draft(&self, draft_id: &DraftId) -> Result<StoredDraft, RuntimeError> {
        Ok(self.store.load_draft(draft_id)?)
    }

    pub fn drafts(&self) -> Result<Vec<StoredDraft>, RuntimeError> {
        Ok(self
            .store
            .load_draft_ids()?
            .iter()
            .map(|draft_id| self.store.load_draft(draft_id))
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// 删掉一个草稿。已经删掉的再删一次不报错。
    pub fn delete_draft(&self, draft_id: &DraftId) -> Result<(), RuntimeError> {
        self.store.delete_draft(draft_id)?;
        tracing::info!(draft = %draft_id, "draft deleted");
        Ok(())
    }

    /// 草稿预览（第 2.4 节）：校验结果、批次表、格子表、立即成交的订单和确认令牌。
    ///
    /// 不落库、不下单。合约规则和费率按现在问到的值算；校验不过时只有原因，
    /// 没有表格也没有令牌。
    ///
    /// 标的上已有仓位时，表格是接管之后的样子（第 8 节）：仓位从靠近空隙的一端分给批次和
    /// 格子，并列出启动后第一步会挂出的订单。接管不了（方向没有对应的一侧、数量超过上限）
    /// 时校验不过。交易所上这个标的已有的挂单都不是本策略的，一并列出、不撤销。
    ///
    /// 令牌带上预览时刻，确认启动时按它判断这份预览有多旧（[`PREVIEW_TTL`]）。
    pub async fn preview_draft(&self, draft_id: &DraftId) -> Result<DraftPreview, RuntimeError> {
        let draft = self.store.load_draft(draft_id)?;
        let instrument = draft.config.instrument.clone();
        let spec = self.account.instrument_spec(&instrument).await?;
        let fees = self.account.fees(&instrument).await?;
        let market_price = self.exchange.mark_price(&instrument).await?;
        let position = self.exchange.position(&instrument).await?;
        let foreign_orders = self.exchange.working_orders(&instrument).await?;
        let available_collateral = self.account.available_collateral().await?;
        let now = self.clock.now();

        let version = self.store.latest_config_version(draft_id)?;
        let mut preview = DraftPreview {
            draft_id: draft_id.clone(),
            instrument,
            config_version: version,
            validation: Validation {
                valid: true,
                reason: None,
            },
            market_price,
            position,
            available_collateral,
            batches: Vec::new(),
            cells: Vec::new(),
            immediate_fills: Vec::new(),
            planned_orders: Vec::new(),
            stress: Vec::new(),
            warnings: Vec::new(),
            foreign_orders,
            token: None,
        };

        let strategy = match Strategy::new(draft.config, spec, fees) {
            Ok(strategy) => strategy,
            Err(error) => {
                preview.validation = Validation {
                    valid: false,
                    reason: Some(error.to_string()),
                };
                return Ok(preview);
            }
        };
        let state = match strategy.takeover_state(position) {
            Ok(state) => state,
            Err(error) => {
                preview.validation = Validation {
                    valid: false,
                    reason: Some(error.to_string()),
                };
                return Ok(preview);
            }
        };
        preview.batches = strategy.batch_summaries(&state);
        preview.cells = strategy.cell_summaries(&state, &[]);
        preview.immediate_fills = strategy.immediate_fills(&state, market_price);
        preview.planned_orders = strategy.plan(&state, &[], &[], market_price).place;

        // 满仓压力测试（第 7.3 节）：达到停止阈值就拒绝这份配置，超过提醒阈值给出提醒。
        let stress = match self
            .stress_preview(&strategy, &state, position, market_price)
            .await
        {
            Ok(stress) => stress,
            Err(error) => {
                preview.validation = Validation {
                    valid: false,
                    reason: Some(format!(
                        "读不到保证金数据，做不了满仓压力测试（{error}），稍后再预览。"
                    )),
                };
                return Ok(preview);
            }
        };
        preview.stress = stress.sides;
        preview.warnings = stress.warnings;
        if let Some(reason) = stress.refusal {
            preview.validation = Validation {
                valid: false,
                reason: Some(reason),
            };
            return Ok(preview);
        }
        preview.token = Some(preview_token(
            &preview,
            strategy.config(),
            strategy.spec(),
            strategy.fees(),
            now,
        )?);
        Ok(preview)
    }

    /// 满仓压力测试（第 7.3 节）：每一侧满仓、价格到最不利位置时的账户保证金率和强平价。
    ///
    /// 账户底数按现在读到的：余额、逐仓保证金，以及这个标的之外的全仓维持保证金占用
    /// （账户全仓占用减去这个标的当前仓位的维持保证金——标的上已有的仓位接管之后算在本策略
    /// 的满仓里，不能再算一遍）。
    async fn stress_preview(
        &self,
        strategy: &Strategy,
        start: &StrategyState,
        position: Position,
        market_price: Price,
    ) -> Result<StressOutcome, RuntimeError> {
        let settings = self.store.load_account_settings()?;
        let instrument = strategy.spec().instrument();
        // 只支持全仓：压力测试和保证金率都按整个账户共用余额来算，逐仓仓位有自己的强平线。
        if !self.account.leverage(instrument).await?.cross {
            return Ok(StressOutcome {
                sides: Vec::new(),
                warnings: Vec::new(),
                refusal: Some(format!(
                    "`{instrument}` 是逐仓，只支持全仓：在交易所改成全仓后再预览。"
                )),
            });
        }
        let margin = self
            .account
            .margin(&self.margin_instruments(Some(instrument))?)
            .await?;
        // 读数和运行时一样要新鲜：拿一份旧的高余额读数做压力测试，结论会偏乐观。
        if let Some(as_of) = margin.as_of
            && !fresh(as_of, self.clock.now(), settings.market_data_max_age)
        {
            return Err(RuntimeError::Exchange(ExchangeError::Unavailable {
                reason: format!("the margin reading taken at {as_of} is not fresh"),
            }));
        }
        let tiers = self.account.margin_tiers(instrument).await?;
        let used = margin.cross_maintenance.max(Notional::from_scaled(
            margin.balance.scaled_floor() - margin.available_after_maintenance.scaled_floor(),
        ));
        let current = tiers.maintenance_margin(Notional::of(
            Quantity::from_units(position.units().abs()),
            market_price,
        ));
        let basis = AccountBasis {
            balance: margin.balance,
            isolated_margin: margin.isolated_margin,
            other_maintenance: Notional::from_scaled(
                (used.scaled_floor() - current.scaled_floor()).max(0),
            ),
        };
        let sides = strategy.stress_test(start, market_price, &basis, &tiers);
        let mut warnings = Vec::new();
        let mut refusal = None;
        for side in &sides {
            if side.margin_ratio < settings.margin_alert_ratio {
                continue;
            }
            // 建议：保证金率在提醒阈值以内的最大仓位；已有仓位本身就超过时给不出建议。
            let suggestion = match strategy.suggest_max_position(
                side.direction,
                position,
                market_price,
                &basis,
                &tiers,
                settings.margin_alert_ratio,
            ) {
                Suggestion::Max(suggested) => format!(
                    "建议最大仓位 M ≤ {suggested}（保证金率不超过 {}）。",
                    percent(settings.margin_alert_ratio)
                ),
                Suggestion::HeldPositionTooLarge => "已有仓位本身就超过提醒阈值：先处理已有仓位或增加资金。".to_string(),
                Suggestion::NoFeasiblePosition => "找不到既够最低下单金额、又不超过提醒阈值的最大仓位：调整范围、间距 d 或主仓比例 r，或增加资金。".to_string(),
            };
            let side_name = side.direction.side_label();
            // 保证金率到了 100% 就是强平：这时报强平价比报一个封顶的比率更有用。
            let what = match side.liquidation {
                Liquidation::At { price: liquidation }
                    if side.margin_ratio >= Rate::from_units(SCALE) =>
                {
                    format!(
                        "{side_name}满仓 {}，价格到 {} 之前就会强平（强平价约 {liquidation}）",
                        side.position, side.adverse_price
                    )
                }
                _ => format!(
                    "{side_name}满仓 {}、价格到 {} 时保证金率 {}",
                    side.position,
                    side.adverse_price,
                    percent(side.margin_ratio)
                ),
            };
            if side.margin_ratio >= settings.margin_stop_ratio {
                refusal.get_or_insert(format!(
                    "{what}，达到停止阈值 {}，运行中加不满。{suggestion}",
                    percent(settings.margin_stop_ratio)
                ));
            } else {
                warnings.push(format!(
                    "{what}，超过提醒阈值 {}。{suggestion}",
                    percent(settings.margin_alert_ratio)
                ));
            }
        }
        Ok(StressOutcome {
            sides,
            warnings,
            refusal,
        })
    }

    /// 确认启动：校验令牌、检查同一标的是否已有策略，然后写入策略记录（B42、B43、B46）。
    ///
    /// 令牌对不上说明预览之后仓位、可用抵押或会立即成交的订单变了，或者这份预览已经
    /// 超过 [`PREVIEW_TTL`]：旧预览说明不了现在会发生什么，要重新预览（B43）。
    /// 同一标的已有运行中或暂停的策略时拒绝启动（B46）。
    pub async fn start_draft(
        &self,
        draft_id: &DraftId,
        token: &str,
    ) -> Result<StrategyId, RuntimeError> {
        // 草稿标识和策略标识共用一套，所以这里也用它互斥：同一个草稿并发确认两次，
        // 只有一次能读到草稿并把它换成策略。
        let _guard = self.begin(draft_id)?;
        let now = self.clock.now();
        let draft = self.store.load_draft(draft_id)?;
        let instrument = draft.config.instrument.clone();
        let spec = self.account.instrument_spec(&instrument).await?;
        let fees = self.account.fees(&instrument).await?;
        let strategy = Strategy::new(draft.config, spec, fees)?;

        let (previewed_at, previewed_collateral, fingerprint) =
            parse_token(token).ok_or(RuntimeError::PreviewStale("确认令牌无法识别"))?;
        let fresh = self.preview_draft(draft_id).await?;
        let fresh_fingerprint =
            preview_fingerprint(&fresh, strategy.config(), strategy.spec(), strategy.fees())?;
        if fresh.token.is_none() {
            return Err(RuntimeError::PreviewStale("参数不再通过校验"));
        }
        if fresh_fingerprint != fingerprint {
            return Err(RuntimeError::PreviewStale("仓位或立即成交的订单变了"));
        }
        if collateral_dropped(previewed_collateral, fresh.available_collateral) {
            return Err(RuntimeError::PreviewStale("可用资金比预览时少"));
        }
        // 时长落在未来（时钟被回拨，或者令牌里的时刻是编的）也算过期：只判「大于 60 秒」
        // 的话，负的时长永远不大于它，60 秒这条上界就被绕过去了。
        let age = now.signed_duration_since(previewed_at);
        if age > PREVIEW_TTL || age < Duration::zero() {
            return Err(RuntimeError::PreviewStale("超过 60 秒"));
        }

        // 和预览同一份起始状态：标的上已有仓位时就是接管之后的状态（第 8 节）。预览校验
        // 已经通过，指纹也对上了，这里不会再接管失败。
        let initial_state = strategy.takeover_state(fresh.position)?;
        self.store
            .start_draft(
                draft_id,
                &StoredStrategy {
                    strategy_id: draft_id.clone(),
                    instrument,
                    status: StrategyStatus::Running,
                    config: strategy.config().clone(),
                    spec: strategy.spec().clone(),
                    fees: *strategy.fees(),
                    fills_synced_through: now,
                    created_at: now,
                    updated_at: now,
                },
                &initial_state,
                // 第一份视图快照就是确认启动这一刻读到的数：列表和详情从策略存在的那
                // 一刻起就有东西可显示，不必等第一步走完。
                &ViewSnapshot {
                    taken_at: now,
                    status: StrategyStatus::Running,
                    note: None,
                    market_price: fresh.market_price,
                    position: fresh.position,
                    expected_position: strategy.expected_position(&initial_state),
                    available_collateral: fresh.available_collateral,
                    margin_ratio: None,
                    waiting: Vec::new(),
                },
            )
            .map_err(|error| match error {
                // 同一个标的上已经有活动策略。检查在写入的那个事务里做，所以两个草稿
                // 同时确认也只有一个能建成（B46）。
                StoreError::InstrumentBusy {
                    instrument,
                    strategy_id,
                } => RuntimeError::InstrumentBusy {
                    instrument,
                    strategy_id,
                },
                other => RuntimeError::Store(other),
            })?;
        tracing::info!(
            strategy = %draft_id,
            instrument = strategy.spec().instrument().symbol(),
            sides = strategy.sides().len(),
            "strategy started from draft"
        );
        Ok(draft_id.clone())
    }

    /// 运行中修改参数：只有限速间隔和观点失效设置可以改（第 2.4 节）。
    ///
    /// 新配置按策略的标的与费率规整后，除了这两项必须和现有配置完全一致：改价格
    /// 范围、比例、批数或网格间距都会让已有的批次划分和格子状态对不上，必须停下来
    /// 重建。改动立刻生效——下一步读的就是新配置——并记一个新的参数版本。
    ///
    /// 加上限速也是改同一个参数：只重建「从不限速变成限速」的那几侧的放行进度，限速
    /// 从这一刻开始计时。已经开始的单位保持放行，还没开始的单位回到未放行——它们在场
    /// 上的订单留在原地，等重新放行之后再由规划处理（第 4.1 节）。另一侧没被改过，
    /// 放行进度和计时完全不动：重建会把它退回「最后一个已经开始的单位」并重新计时，
    /// 白白推后它下一次放行。
    ///
    /// 改配置和走一步都会读写同一份状态，所以两者互斥：正在被驱动时排队等这一步走完，
    /// 等太久才返回 [`RuntimeError::Busy`]。
    pub async fn update_config(
        &self,
        strategy_id: &StrategyId,
        config: StrategyConfig,
    ) -> Result<i64, RuntimeError> {
        let _guard = self.begin_waiting(strategy_id).await?;
        let now = self.clock.now();
        self.refuse_archived(strategy_id)?;
        let stored = self.store.load_strategy(strategy_id)?;
        // 只有运行中的策略能改参数：暂停中/停止中在撤单收尾，恢复中还没恢复成功，
        // 改了也不会生效，只会让用户以为改动被接受了。
        if stored.status != StrategyStatus::Running {
            return Err(RuntimeError::NotRunning(strategy_id.clone(), stored.status));
        }
        let candidate = Strategy::new(config, stored.spec.clone(), stored.fees)?;
        let new_config = candidate.config().clone();
        if let Some(field) = changed_field(&stored.config, &new_config) {
            return Err(RuntimeError::ConfigChangeNotAllowed { field });
        }
        if stored.config == new_config {
            // 一个参数都没变（比如网页上连点两次保存）：不记新版本，报当前版本号。
            // 参数版本是事后复盘「哪次改动让成交变了」的依据，同样的参数记两遍会让
            // 版本号不再对应不同的参数集。
            return Ok(self.store.latest_config_version(strategy_id)?);
        }
        let throttled: Vec<Direction> = Direction::ALL
            .into_iter()
            .filter(|direction| gained_throttle(&stored.config, &new_config, *direction))
            .collect();
        // 限速进度不属于配置，加上限速时要单独重建：按修改这一刻重建放行进度。它和新
        // 配置写在同一笔里——分成两次写，中间崩掉就留下「配置已经限速、进度还是全部
        // 放行」，重启后限速静默失效（第 4.1 节）。
        let state = if throttled.is_empty() {
            None
        } else {
            let mut state = self.load_state(strategy_id, &candidate)?;
            candidate.rebuild_throttle(&mut state, now, &throttled);
            Some(state)
        };
        let version = self
            .store
            .save_config(strategy_id, &new_config, state.as_ref(), now)?;
        tracing::info!(
            strategy = %strategy_id,
            version,
            long_throttle_interval = ?new_config.long.as_ref().and_then(|side| side.throttle_interval),
            short_throttle_interval = ?new_config.short.as_ref().and_then(|side| side.throttle_interval),
            "config updated"
        );
        Ok(version)
    }

    /// 账户风控设置：保证金率的提醒阈值与停止阈值、行情过期时间（第 7.4、11 节）。
    pub fn account_settings(&self) -> Result<AccountSettings, RuntimeError> {
        Ok(self.store.load_account_settings()?)
    }

    /// 修改账户风控设置。校验不过时拒绝；保存之后下一步就按新设置判断。
    pub fn update_account_settings(&self, settings: AccountSettings) -> Result<(), RuntimeError> {
        settings.validate()?;
        self.store.save_account_settings(&settings)?;
        tracing::info!(
            alert = %settings.margin_alert_ratio,
            stop = %settings.margin_stop_ratio,
            market_data_max_age = settings.market_data_max_age.num_seconds(),
            "account settings updated"
        );
        Ok(())
    }

    /// 一个策略的参数版本，按版本号从小到大。
    pub fn config_versions(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<Vec<StoredConfigVersion>, RuntimeError> {
        Ok(self.store.load_config_versions(strategy_id)?)
    }
}

/// 预览的有效期：超过这段时间就要重新预览（第 2.4 节）。
///
/// 预览说的是「现在会发生什么」：仓位、可用抵押和会立即成交的订单都可能在这段时间里
/// 变了。标记价本身不算——它每时每刻都在动，只看它等于永远要求重新预览；要看的是它
/// 算出来的结果，也就是会立即成交的那份名单。
pub(super) const PREVIEW_TTL: Duration = Duration::seconds(60);

/// 确认令牌：这次预览说明的那些事实的指纹，加上预览的时刻和当时的可用抵押。
///
/// 指纹覆盖参数、合约规则、费率、算出来的表格、第一步要挂的单和仓位——它们里的任何
/// 一个变了，旧预览就不再说明现在会发生什么，确认启动会被要求重新预览（B43）。
///
/// 可用抵押不进指纹，单独比（[`collateral_dropped`]）：标的上有仓位时，统一账户的可用
/// 抵押随浮动盈亏每时每刻都在变，进指纹就等于有仓位时永远确认不了——接管已有仓位正是
/// 这种情形。
///
/// 标记价本身不进指纹：它每时每刻都在动，进指纹就等于永远要求重新预览。要看的是它
/// 算出来的结果——哪些订单会立即成交。标记价变了但这份名单没变，预览仍然是准的。
///
/// 令牌里带上预览时刻，确认启动时按它判断预览有多旧（[`PREVIEW_TTL`]）。
pub(super) fn preview_token(
    preview: &DraftPreview,
    config: &StrategyConfig,
    spec: &InstrumentSpec,
    fees: &Fees,
    previewed_at: DateTime<Utc>,
) -> Result<String, StoreError> {
    let fingerprint = preview_fingerprint(preview, config, spec, fees)?;
    Ok(format!(
        "{}.{}.{fingerprint}",
        previewed_at.timestamp_millis(),
        preview.available_collateral.scaled_floor()
    ))
}

/// 从令牌里取出预览时刻、当时的可用抵押和指纹。
///
/// 令牌是网页原样传回来的，解析不出来就当过期处理：重新预览一次没有代价，
/// 拿一个看不懂的令牌去启动才是问题。
pub(super) fn parse_token(token: &str) -> Option<(DateTime<Utc>, Notional, &str)> {
    let mut parts = token.splitn(3, '.');
    let millis: i64 = parts.next()?.parse().ok()?;
    let collateral: i64 = parts.next()?.parse().ok()?;
    let fingerprint = parts.next()?;
    if fingerprint.is_empty() {
        return None;
    }
    Some((
        DateTime::from_timestamp_millis(millis)?,
        Notional::from_scaled(collateral),
        fingerprint,
    ))
}

/// 预览之后可用抵押少了多少才算预览过期：少于预览时的 99%。
///
/// 多了没关系，预览说明的那些单照样下得出去；小幅波动是有仓位时的常态（浮动盈亏）。
/// 明显变少——用户在别处用掉了资金、或者仓位亏损扩大——预览时看到的资金状况就不再成立，
/// 要重新预览（B43）。
const COLLATERAL_TOLERANCE_PERCENT: i128 = 1;

fn collateral_dropped(previewed: Notional, fresh: Notional) -> bool {
    // 令牌里存的是按 1e-8 取整的值，两边按同一精度比。
    let fresh = i128::from(fresh.scaled_floor());
    let previewed = i128::from(previewed.scaled_floor());
    fresh * 100 < previewed * (100 - COLLATERAL_TOLERANCE_PERCENT)
}

/// 预览说明的那些事实的指纹。
pub(super) fn preview_fingerprint(
    preview: &DraftPreview,
    config: &StrategyConfig,
    spec: &InstrumentSpec,
    fees: &Fees,
) -> Result<String, StoreError> {
    let parts = [
        preview.draft_id.as_str().to_string(),
        preview.config_version.to_string(),
        serde_json::to_string(config)?,
        serde_json::to_string(spec)?,
        serde_json::to_string(fees)?,
        serde_json::to_string(&preview.batches)?,
        serde_json::to_string(&preview.cells)?,
        serde_json::to_string(&preview.immediate_fills)?,
        serde_json::to_string(&preview.planned_orders)?,
        preview.position.to_string(),
        // 提醒的条数：确认前从「没有提醒」变成「有提醒」，用户没看到这条提醒，要重新预览。
        // 不放提醒的原文：里面的比率随账户每时每刻都在变。
        preview.warnings.len().to_string(),
    ];
    let mut hasher = Sha3_256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update(b"\n");
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 除限速间隔之外，新配置改动了的第一个字段。
///
/// 这些参数决定了批次划分和格子划分，运行中改动会让已保存的状态对不上，只能在
/// 停止后重建（第 2.4 节）。返回字段名是为了让报错说清楚改了什么；两侧都看，
/// 哪一侧先被改动就报哪一侧（先多头侧、后空头侧）。
///
/// 标的没有列在这里：新配置的标的必须和交易所的一致，规整时就会以配置错误拒绝。
pub(super) fn changed_field(old: &StrategyConfig, new: &StrategyConfig) -> Option<&'static str> {
    for direction in Direction::ALL {
        let (old_side, new_side) = (old.side(direction), new.side(direction));
        if old_side.is_some() != new_side.is_some() {
            return Some("sides");
        }
        let (Some(old_side), Some(new_side)) = (old_side, new_side) else {
            continue;
        };
        // 先逐项比较，好报出具体是哪个字段被改了。
        if old_side.price_low != new_side.price_low {
            return Some("price_low");
        }
        if old_side.price_high != new_side.price_high {
            return Some("price_high");
        }
        if old_side.max_position != new_side.max_position {
            return Some("max_position");
        }
        if old_side.core_ratio != new_side.core_ratio {
            return Some("core_ratio");
        }
        if old_side.batch_count != new_side.batch_count {
            return Some("batch_count");
        }
        if old_side.core_skew != new_side.core_skew {
            return Some("core_skew");
        }
        if old_side.grid_spacing != new_side.grid_spacing {
            return Some("grid_spacing");
        }
        if old_side.grid_spread != new_side.grid_spread {
            return Some("grid_spread");
        }
    }
    // 兜底：以后给配置加了新参数而忘了在这里列出来，也必须是「拒绝」而不是「悄悄
    // 生效」——放行一个决定批次与格子划分的参数会让状态和配置对不上。比较的是整个
    // 配置，所以将来加在顶层的参数也挡得住。
    if without_throttle(new) != without_throttle(old) {
        return Some("unlisted");
    }
    None
}

/// 去掉两侧的限速参数：限速随时可以改，不参与「配置有没有被改过」的比较。
pub(super) fn without_throttle(config: &StrategyConfig) -> StrategyConfig {
    let mut cleared = config.clone();
    for side in [&mut cleared.long, &mut cleared.short]
        .into_iter()
        .flatten()
    {
        side.throttle_interval = None;
        // 失效设置和限速一样随时可以改（第 2.4 节），不参与「有没有改了不能改的参数」。
        side.invalidation = None;
    }
    cleared
}

/// 这一侧是不是「从不限速变成限速」。
///
/// 只有这样的侧需要重建放行进度。本来就有间隔的侧只是换了间隔长度，放行进度和计时
/// 照旧；不限速的侧没有进度可言。已经限速的侧改成不限速也不需要重建：规划对不限速
/// 的侧本来就不看放行进度，下一次走步会自己把状态写成不限速的样子。
pub(super) fn gained_throttle(
    previous: &StrategyConfig,
    next: &StrategyConfig,
    direction: Direction,
) -> bool {
    let interval = |config: &StrategyConfig| {
        config
            .side(direction)
            .and_then(|side| side.throttle_interval)
    };
    interval(previous).is_none() && interval(next).is_some()
}

/// 预览里的满仓压力测试结果。
struct StressOutcome {
    sides: Vec<SideStress>,
    warnings: Vec<String>,
    /// 达到停止阈值时拒绝这份配置的原因。
    refusal: Option<String>,
}
