//! 网页读的列表与详情，以及每一步之后落的视图快照（第 11 节）。

use chrono::{DateTime, Utc};
use poise_strategy::Strategy;
use poise_strategy::invalidation::reduction_sold;
use poise_strategy::order::{OrderPurpose, OrderRecord, Unit, WorkingOrder};
use poise_strategy::planning::WaitReason;
use poise_strategy::preview::{BatchSummary, CellSummary, HoldingCost};
use poise_strategy::side::Direction;
use poise_strategy::units::{Notional, Position, Quantity, Rate, SCALE};

use crate::account::FundingRate;
use crate::settings::AccountSettings;
use crate::store::{StoredFill, StoredOrder, StrategyId, StrategyStatus, ViewSnapshot};

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
        let strategy = Strategy::new(stored.config.clone(), stored.spec.clone(), stored.fees)?;
        let orders = self.store.load_orders(strategy_id)?;
        // 观点失效卖出的数量不进账本：批次表和格子表按「先网格、后主仓」把它扣掉再显示
        // （第 7.5 节，见 [`Strategy::after_invalidation`]）。
        let records: Vec<OrderRecord> = orders.iter().map(|order| order.record.clone()).collect();
        let sold = reduction_sold(&records);
        let ledger = self.load_state(strategy_id, &strategy)?;
        let state = strategy.after_invalidation(&ledger, &sold);
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
        // 资金费只算这个策略的那一段：建立之后，归档的到归档为止（第 7.6 节）。
        let funding_end = status.is_archived().then_some(summary.updated_at);
        let funding_total =
            self.store
                .funding_total(&stored.instrument, stored.created_at, funding_end)?;
        let exits: Vec<(Unit, Quantity)> = orders
            .iter()
            .filter(|order| order.record.purpose == OrderPurpose::GridExit)
            .map(|order| (order.record.unit, order.record.cumulative_filled))
            .collect();
        let grid_realized_profit = strategy.grid_realized_profit(&exits);
        let batches = strategy.batch_summaries(&state);
        let cells = strategy.cell_summaries(&state, &working);
        let holding = HoldingCost::of_tables(&batches, &cells);
        let reduction_realized_profit = reduction_realized(
            &strategy.batch_summaries(&ledger),
            &strategy.cell_summaries(&ledger, &working),
            &batches,
            &cells,
            &orders,
            &self.store.load_fills(strategy_id)?,
        );
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
            market_price: view.market_price,
            working_orders: orders
                .iter()
                .filter(|order| order.record.is_working())
                .count(),
            held: self.held_position(&stored),
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

/// 失效减仓和市价平仓已经实现的盈亏（第 7.5、7.7 节），两侧分开算：成交得到的钱减去卖掉
/// 那部分的账本成本（减仓之前的账本成本减去之后的）。还没有减仓成交时是 `None`。
///
/// 市价平仓平的是整个标的的净仓位，可能比账本上多（用户手动开的仓位，或者归档之后再平掉
/// 的仓位）：多出来的那部分不是这个策略的。按发出的先后，每张单只有还没超出账本减掉的
/// 数量的那部分成交算本策略的，成交金额按这部分占比折算。
fn reduction_realized(
    batches_before: &[BatchSummary],
    cells_before: &[CellSummary],
    batches_after: &[BatchSummary],
    cells_after: &[CellSummary],
    orders: &[StoredOrder],
    fills: &[StoredFill],
) -> Option<Notional> {
    let mut reductions: Vec<&StoredOrder> = orders
        .iter()
        .filter(|order| {
            order.record.purpose.is_reduction() && !order.record.cumulative_filled.is_zero()
        })
        .collect();
    if reductions.is_empty() {
        return None;
    }
    reductions.sort_by_key(|order| order.placed_at);
    let realized = Direction::ALL
        .into_iter()
        .map(|direction| {
            let before = side_cost(direction, batches_before, cells_before);
            let after = side_cost(direction, batches_after, cells_after);
            let mut left = (before.position.units() - after.position.units()).abs();
            let mut amount = Notional::ZERO;
            for order in reductions
                .iter()
                .filter(|order| order.record.unit.direction() == direction)
            {
                let (proceeds, quantity) = order_proceeds(&order.record, fills);
                let taken = quantity.units().min(left);
                left -= taken;
                amount = amount + scale(proceeds, taken, quantity.units());
            }
            // 多头侧卖出收钱，空头侧买回花钱。
            let signed = match direction {
                Direction::Long => amount,
                Direction::Short => Notional::ZERO - amount,
            };
            signed - (before.cost - after.cost)
        })
        .fold(Notional::ZERO, |total, realized| total + realized);
    Some(realized)
}

/// 一侧的账本持仓成本。
fn side_cost(direction: Direction, batches: &[BatchSummary], cells: &[CellSummary]) -> HoldingCost {
    HoldingCost::of(
        batches
            .iter()
            .filter(|batch| batch.direction == direction)
            .map(|batch| (direction, batch.core_filled, batch.price))
            .chain(
                cells
                    .iter()
                    .filter(|cell| cell.direction == direction)
                    .map(|cell| (direction, cell.held, cell.entry_price)),
            ),
    )
}

/// 一张减仓单成交的金额和数量（累计成交量）。
///
/// 按这张单记下的成交流水的价格算。流水只往回拉一段，可能比累计成交量少（服务停过很久）：
/// 少的那部分按这张单已有流水的均价算，一条流水都没有时按限价算。
fn order_proceeds(record: &OrderRecord, fills: &[StoredFill]) -> (Notional, Quantity) {
    let (amount, quantity) = fills
        .iter()
        .filter(|fill| fill.client_order_id == record.client_order_id)
        .fold(
            (Notional::ZERO, Quantity::ZERO),
            |(amount, quantity), fill| {
                (
                    amount + Notional::of(fill.quantity, fill.price),
                    quantity + fill.quantity,
                )
            },
        );
    let filled = record.cumulative_filled.max(quantity);
    let amount = if quantity.is_zero() {
        Notional::of(filled, record.limit_price)
    } else {
        scale(amount, filled.units(), quantity.units())
    };
    (amount, filled)
}

/// `amount × part ÷ whole`，先除后乘，不让中间结果溢出；`whole` 为 0 时是 0。
fn scale(amount: Notional, part: i64, whole: i64) -> Notional {
    if whole == 0 {
        return Notional::ZERO;
    }
    let (part, whole) = (i128::from(part), i128::from(whole));
    let raw = amount.raw();
    Notional::from_raw(raw / whole * part + raw % whole * part / whole)
}

/// `part` 占 `whole` 的比例（带符号）；`whole` 不是正数、或者算出来超出范围时是 `None`。
fn share_of(part: Notional, whole: Notional) -> Option<Rate> {
    if whole <= Notional::ZERO {
        return None;
    }
    let units = part.raw().checked_mul(i128::from(SCALE))? / whole.raw();
    i64::try_from(units).ok().map(Rate::from_units)
}
