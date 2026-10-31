//! 网页读的列表与详情，以及每一步之后落的视图快照（第 11 节）。

use chrono::{DateTime, Utc};
use poise_strategy::Strategy;
use poise_strategy::invalidation::reduction_sold;
use poise_strategy::order::{OrderRecord, WorkingOrder};
use poise_strategy::planning::WaitReason;
use poise_strategy::pnl::{Execution, holdings, profit_and_loss};
use poise_strategy::side::Direction;
use poise_strategy::state::StrategyState;
use poise_strategy::units::{Notional, Position, Price, Quantity, Rate, SCALE};

use crate::account::FundingRate;
use crate::settings::AccountSettings;
use crate::store::{
    StoredFill, StoredOrder, StoredStrategy, StrategyId, StrategyStatus, ViewSnapshot,
};

use super::{
    InstanceInfo, Runtime, RuntimeError, StepReport, StrategyDetail, StrategySummary, percent,
};

impl Runtime {
    /// 这个实例连的网络和账户（页面顶部的标识）。
    pub fn instance(&self) -> Result<InstanceInfo, RuntimeError> {
        let identity = self.store.identity()?;
        Ok(InstanceInfo {
            network: identity.as_ref().map(|(network, _)| network.clone()),
            account_address: identity.map(|(_, account)| account),
        })
    }

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
        let orders = self.store.load_orders(strategy_id)?;
        let Ledger {
            strategy,
            ledger,
            state,
            working,
        } = self.ledger(strategy_id, &stored, &orders)?;
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
        // 资金费只算这个策略的那一段：建立之后，归档的到归档为止（第 7.6 节）。
        let funding_end = status.is_archived().then_some(summary.updated_at);
        let funding_total =
            self.store
                .funding_total(&stored.instrument, stored.created_at, funding_end)?;
        let batches = strategy.batch_summaries(&state);
        let cells = strategy.cell_summaries(&state, &working);
        let pnl = profit_and_loss(
            &holdings(
                &strategy.batch_summaries(&ledger),
                &strategy.cell_summaries(&ledger, &working),
            ),
            &holdings(&batches, &cells),
            &executions(&strategy, &orders, &self.store.load_fills(strategy_id)?),
        );
        let grid_realized_profit = pnl.grid_realized;
        let reduction_realized_profit = pnl.reduction_realized;
        let holding = pnl.holding;
        let realized = grid_realized_profit
            + reduction_realized_profit.unwrap_or(Notional::ZERO)
            + funding_total;
        let unrealized_pnl = holding.unrealized(view.market_price);
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
            batches,
            cells,
            orders: orders
                .into_iter()
                .filter(|order| order.record.is_working())
                .collect(),
            waiting: view.waiting,
            config_versions: self.store.load_config_versions(strategy_id)?,
            actions_today: self.actions_today()?,
            funding_rate: funding.map(|(rate, _)| rate),
            funding_rate_read_at: funding.map(|(_, read_at)| read_at),
            funding_total,
            funding_synced_through: self.store.funding_synced_through(&stored.instrument)?,
            grid_realized_profit,
            funding_share: share_of(funding_total, grid_realized_profit),
            average_price: holding.average_price(),
            unrealized_pnl,
            reduction_realized_profit,
            total_pnl: realized + unrealized_pnl,
            breakeven_price: holding.breakeven_price(realized),
            backstops: self.backstop_views(strategy_id)?,
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
        let tables = self.ledger(strategy_id, &stored, &orders)?;
        let cells = tables
            .strategy
            .cell_summaries(&tables.state, &tables.working);
        Ok(StrategySummary {
            strategy_id: strategy_id.clone(),
            instrument: stored.instrument.clone(),
            status: stored.status,
            config_version: self.store.latest_config_version(strategy_id)?,
            created_at: stored.created_at,
            updated_at: stored.updated_at,
            position: view.position,
            market_price: view.market_price,
            working_orders: orders
                .iter()
                .filter(|order| order.record.is_working())
                .count(),
            held: self.held_position(&stored),
            config: stored.config,
            cells,
        })
    }

    /// 这个策略的账本：库里记的状态，和扣掉失效减仓、市价平仓之后给页面看的那份。
    ///
    /// 观点失效卖出的数量不进账本：批次表和格子表按「先网格、后主仓」把它扣掉再显示
    /// （第 7.5 节，见 [`Strategy::after_invalidation`]）。
    fn ledger(
        &self,
        strategy_id: &StrategyId,
        stored: &StoredStrategy,
        orders: &[StoredOrder],
    ) -> Result<Ledger, RuntimeError> {
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let records: Vec<OrderRecord> = orders.iter().map(|order| order.record.clone()).collect();
        let sold = reduction_sold(&records);
        let ledger = self.load_state(strategy_id, &strategy)?;
        let state = strategy.after_invalidation(&ledger, &sold);
        let working = orders
            .iter()
            .filter(|order| order.record.is_working())
            .map(|order| order.record.working_view())
            .collect();
        Ok(Ledger {
            strategy,
            ledger,
            state,
            working,
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

    /// 和 [`Runtime::annotate_view`] 一样只改状态和说明，另外把「这一步在等什么」换掉。
    ///
    /// 运行中的一步读取没走通时用：上一份快照里的等待原因（往往是「没有等待」）已经不
    /// 成立，留着它页面就会一边说「该挂的单都挂上了」、一边说这一步没读到数据。
    pub(super) async fn annotate_view_waiting(
        &self,
        strategy_id: &StrategyId,
        status: StrategyStatus,
        note: String,
        waiting: Vec<WaitReason>,
    ) -> Result<(), RuntimeError> {
        let mut view = self.store.load_view(strategy_id)?;
        view.status = status;
        view.note = Some(note);
        view.waiting = waiting;
        self.store.save_view(strategy_id, &view)?;
        Ok(())
    }
}

/// [`Runtime::ledger`] 的结果。
struct Ledger {
    strategy: Strategy,
    /// 库里记的状态。
    ledger: StrategyState,
    /// 扣掉减仓之后的状态。
    state: StrategyState,
    working: Vec<WorkingOrder>,
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
            "资金费率不利：年化 {}（阈值 {}）。",
            percent(paid),
            percent(settings.funding_alert_annual_rate)
        )
    })
}

/// 这个策略每张订单的成交（第 7.6 节）。
///
/// 成交价取成交流水。流水只往回拉一段，可能比订单的累计成交量少（服务停过很久）：少的
/// 那部分按限价补一笔，记在下单时刻。交易所没给手续费的（模拟交易所、这个版本之前记的成交），按费率
/// 估算：挂单腿按 maker，失效减仓和市价平仓按 taker。
fn executions(strategy: &Strategy, orders: &[StoredOrder], fills: &[StoredFill]) -> Vec<Execution> {
    let fees = strategy.fees();
    let mut executions = Vec::new();
    for order in orders {
        let record = &order.record;
        let rate = if record.purpose.is_reduction() {
            fees.taker_rate
        } else {
            fees.maker_rate
        };
        let execution = |price: Price, quantity: Quantity, fee: Option<Notional>, at| Execution {
            unit: record.unit,
            purpose: record.purpose,
            price,
            quantity,
            fee: fee.unwrap_or_else(|| Notional::of(quantity, price).mul_rate(rate)),
            at,
        };
        let mut recorded = Quantity::ZERO;
        for fill in fills
            .iter()
            .filter(|fill| fill.client_order_id == record.client_order_id)
        {
            recorded = recorded + fill.quantity;
            executions.push(execution(
                fill.price,
                fill.quantity,
                fill.fee,
                fill.filled_at,
            ));
        }
        // 流水可能领先于账本（先写流水、后记账），这时不补；`saturating_sub` 不在零处截断。
        if record.cumulative_filled > recorded {
            // 补的那笔记在下单时刻：早于这张单之后的平仓，配对顺序才对。
            let missing = record.cumulative_filled.saturating_sub(recorded);
            executions.push(execution(
                record.limit_price,
                missing,
                None,
                order.placed_at,
            ));
        }
    }
    executions
}

/// `part` 占 `whole` 的比例（带符号）；`whole` 不是正数、或者算出来超出范围时是 `None`。
fn share_of(part: Notional, whole: Notional) -> Option<Rate> {
    if whole <= Notional::ZERO {
        return None;
    }
    let units = part.raw().checked_mul(i128::from(SCALE))? / whole.raw();
    i64::try_from(units).ok().map(Rate::from_units)
}
