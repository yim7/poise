//! 网页读的列表与详情，以及每一步之后落的视图快照（第 11 节）。

use chrono::{DateTime, Utc};
use poise_strategy::Strategy;
use poise_strategy::invalidation::invalidation_sold;
use poise_strategy::order::{OrderRecord, WorkingOrder};
use poise_strategy::side::Direction;
use poise_strategy::units::{Notional, Position, Rate};

use crate::account::FundingRate;
use crate::settings::AccountSettings;
use crate::store::{StrategyId, StrategyStatus, ViewSnapshot};

use super::{Runtime, RuntimeError, StepReport, StrategyDetail, StrategySummary, percent};

impl Runtime {
    /// 策略列表，新建的在前（第 11 节）。
    ///
    /// 只读库：仓位来自后台每一步之后落的视图快照（第 11 节）。不去问交易所——网页
    /// 每隔几秒刷新一次，每次都问一遍既慢，又要和常驻服务抢同一把驱动锁。
    pub fn strategies(&self) -> Result<Vec<StrategySummary>, RuntimeError> {
        let mut summaries = Vec::new();
        // `load_all_strategy_ids` 已经是新建的在前。
        for strategy_id in self.store.load_all_strategy_ids()? {
            summaries.push(self.summary(&strategy_id)?);
        }
        Ok(summaries)
    }

    /// 运行详情（第 11 节）：只读库，不下单、不撤单，也不问交易所。
    ///
    /// 页面上的行情、仓位、可用资金和等待原因都来自后台最近一次驱动落的视图快照，
    /// 快照带上取数时刻，页面能看出这份读数有多旧。页面刷得再勤也不会多一次交易所调用，
    /// 也不会挡住正在走的那一步。
    pub fn detail(&self, strategy_id: &StrategyId) -> Result<StrategyDetail, RuntimeError> {
        let summary = self.summary(strategy_id)?;
        let status = summary.status;
        let stored = self.store.load_strategy(strategy_id)?;
        let view = self.store.load_view(strategy_id)?;
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let orders = self.store.load_orders(strategy_id)?;
        // 观点失效卖出的数量不进账本：批次表和格子表按「先网格、后主仓」把它扣掉再显示
        // （第 7.5 节，见 [`Strategy::after_invalidation`]）。
        let records: Vec<OrderRecord> = orders.iter().map(|order| order.record.clone()).collect();
        let sold = invalidation_sold(&records);
        let state = strategy.after_invalidation(&self.load_state(strategy_id, &strategy)?, &sold);
        let working: Vec<WorkingOrder> = orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.record.working_view())
            .collect();
        // 资金费率按读详情这一刻的读数（常驻服务单独去读，半小时没读到就当读不到），不按快照：
        // 暂停的策略不走步，快照不会更新，可仓位还在付资金费（第 7.6 节）。归档的策略不提醒。
        let funding = self.current_funding_rate(&stored.instrument);
        let funding_alert = if status.is_live() {
            funding_alert_note(
                funding.map(|(rate, _)| rate),
                paying_side(&strategy, view.position),
                &self.store.load_account_settings()?,
            )
        } else {
            None
        };
        Ok(StrategyDetail {
            summary,
            taken_at: view.taken_at,
            market_price: view.market_price,
            available_collateral: view.available_collateral,
            margin_ratio: view.margin_ratio,
            position: view.position,
            expected_position: view.expected_position,
            // 暂停原因单独存：撤单收尾的进度说明会覆盖视图里的说明（B30）。
            note: match (status, self.store.load_pause_reason(strategy_id)?) {
                (StrategyStatus::Pausing | StrategyStatus::Paused, Some(reason)) => {
                    Some(match view.note {
                        Some(note) if !note.starts_with(&reason) => format!("{reason}{note}"),
                        Some(note) => note,
                        None => reason,
                    })
                }
                _ => view.note,
            }
            .into_iter()
            .chain(funding_alert)
            .reduce(|note, alert| format!("{note}{alert}")),
            batches: strategy.batch_summaries(&state),
            cells: strategy.cell_summaries(&state, &working),
            orders: orders
                .into_iter()
                .filter(|order| order.record.is_working())
                .collect(),
            waiting: view.waiting,
            config_versions: self.store.load_config_versions(strategy_id)?,
            actions_today: self.actions_today()?,
            funding_rate: funding.map(|(rate, _)| rate),
            funding_rate_read_at: funding.map(|(_, read_at)| read_at),
        })
    }

    /// 列表和详情共用的那一行。
    ///
    /// 仓位取视图快照：它和详情里那份读数是同一次驱动读回来的，页面上的列表和详情
    /// 不会各说各话。在场订单数取订单表——那是本地的记录，比快照新。
    pub(super) fn summary(
        &self,
        strategy_id: &StrategyId,
    ) -> Result<StrategySummary, RuntimeError> {
        let stored = self.store.load_strategy(strategy_id)?;
        let view = self.store.load_view(strategy_id)?;
        let orders = self.store.load_orders(strategy_id)?;
        Ok(StrategySummary {
            strategy_id: strategy_id.clone(),
            instrument: stored.instrument.clone(),
            status: stored.status,
            config_version: self.store.latest_config_version(strategy_id)?,
            created_at: stored.created_at,
            updated_at: stored.updated_at,
            position: view.position,
            working_orders: orders
                .iter()
                .filter(|order| order.record.is_working())
                .count(),
            config: stored.config,
        })
    }

    /// 落一份视图快照（第 11 节）：网页的列表与详情只读它，不再去问交易所。
    ///
    /// 调用方必须在订单、状态都写完盘之后调它——快照里的仓位和在场订单数是给用户看的
    /// 「这一刻的样子」，先落快照再改库就会自相矛盾。
    ///
    /// 读数全部来自这一步读取阶段的那份快照，这里不再问交易所：走到这里单已经下出去、
    /// 撤回来了，再补一次查询失败，就会把做成的一步报成失败，用户看到「暂停失败」而
    /// 单其实已经撤了。
    ///
    /// `note` 是这个策略为什么还没走完当前状态的说明，由调用方给：只撤单的状态说还差
    /// 哪几张单没撤、没对完账，运行中说哪几张单结果未知。页面上只有状态名的话，用户
    /// 看不出它在等什么，也不知道该不该等下去。
    pub(super) async fn save_view(
        &self,
        strategy_id: &StrategyId,
        report: &StepReport,
        available_collateral: Notional,
        margin_ratio: Option<Rate>,
        note: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<(), RuntimeError> {
        let view = ViewSnapshot {
            taken_at: now,
            status: report.status,
            note,
            market_price: report.plan.market_price,
            position: report.position,
            expected_position: report.expected_position,
            available_collateral,
            margin_ratio,
            waiting: report.plan.waiting.clone(),
        };
        self.store.save_view(strategy_id, &view)?;
        Ok(())
    }

    /// 只改状态和说明，读数沿用上一份快照。
    ///
    /// 恢复没走通、还在等订单过期时用：这一刻本来就没有新读数（读取这一步本身就没做成），
    /// 但网页要能看到它在恢复、卡在哪。
    ///
    /// `taken_at` 原样保留：它说的是「这份读数是什么时候取的」。改成现在，用户就会把一份
    /// 十分钟前的仓位当成刚取的——说明文字补不回丢掉的时间（第 11 节）。状态什么时候变的
    /// 另有一处：`strategies.updated_at` 由 [`Store::save_strategy_status`] 写。
    pub(super) async fn annotate_view(
        &self,
        strategy_id: &StrategyId,
        status: StrategyStatus,
        note: String,
    ) -> Result<(), RuntimeError> {
        let mut view = self.store.load_view(strategy_id)?;
        view.status = status;
        view.note = Some(note);
        self.store.save_view(strategy_id, &view)?;
        Ok(())
    }
}

/// 资金费由哪一侧付才算对本策略不利：按实际净仓位的方向；还没有仓位时按单侧网格的方向，
/// 中性网格没有仓位时谁都不付。
pub(super) fn paying_side(strategy: &Strategy, position: Position) -> Option<Direction> {
    match position.units() {
        units if units > 0 => Some(Direction::Long),
        units if units < 0 => Some(Direction::Short),
        _ if strategy.is_neutral() => None,
        _ => strategy.sides().first().map(|side| side.direction()),
    }
}

/// 资金费率对本策略不利、年化超过提醒阈值时的提醒（第 7.6 节）：正费率多头付，负费率
/// 空头付。不自动交易，是否调整由用户决定。
pub(super) fn funding_alert_note(
    rate: Option<FundingRate>,
    side: Option<Direction>,
    settings: &AccountSettings,
) -> Option<String> {
    let annual = rate?.annual.units();
    // 多头付正费率、空头付负费率：换成「本策略付出的年化」再和阈值比。
    let paid = Rate::from_units(match side? {
        Direction::Long => annual,
        Direction::Short => annual.checked_neg()?,
    });
    (paid > settings.funding_alert_annual_rate).then(|| {
        format!(
            "资金费率对本策略不利：年化 {}，超过提醒阈值 {}。不自动调整，是否减仓或改参数由你决定。",
            percent(paid),
            percent(settings.funding_alert_annual_rate)
        )
    })
}
