//! 模拟交易所。
//!
//! 只用于测试：不连接任何外部服务，也不做订单之间的撮合。
//! 价格穿过某张挂单的限价时，这张挂单按自己的限价全部成交。
//!
//! 时间由测试通过 [`SimulatedExchange::set_market_price`] 和
//! [`SimulatedExchange::fill`] 推进；模拟交易所只用一个时间点判断下单是否已过期，
//! 不会自动撤销过期挂单（撤销由 runtime 负责）。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{ClientOrderId, OrderStatus, Side};
use poise_strategy::units::{Notional, Position, Price, Quantity, Rate};

use crate::clock::ManualClock;
use crate::exchange::{
    Exchange, ExchangeError, ExchangeFill, ExchangeOrder, OrderLookup, OrderReceipt, OrderRequest,
    StopRequest, TimeInForce,
};
use crate::store::Store;

pub struct SimulatedExchange {
    instrument: Instrument,
    state: Mutex<SimState>,
}

struct SimState {
    /// 模拟交易所看到的当前时间，只用于判断下单是否已过期。
    now: DateTime<Utc>,
    /// 设了就按这个费率给每笔成交报手续费；没设时成交不带手续费（交易所没给）。
    fee_rate: Option<Rate>,
    /// 下一笔成交按这个价格成交，而不是限价（立即成交时拿到了更好的价格）。
    next_fill_price: Option<Price>,
    market_price: Price,
    position: Position,
    orders: Vec<SimOrder>,
    /// 止损触发单。触发前不在挂单列表里；触发时按标记价立即成交（只减仓），
    /// 成交和普通订单一样进成交流水（见 [`SimulatedExchange::set_market_price`]）。
    stops: Vec<SimStop>,
    /// 下一张止损单被拒的原因。
    reject_next_stop: Option<String>,
    fills: Vec<ExchangeFill>,
    /// 暂时不出现在按时间查询结果里的成交（模拟成交查询有延迟）。
    delayed_fills: Vec<ExchangeFill>,
    delay_next_fills: usize,
    next_order_id: u64,
    next_trade_id: u64,
    reject_next_place: Option<String>,
    /// 下一次下单以操作次数超限被拒的原因。
    rate_limit_next_place: Option<String>,
    /// 还要让几次撤单以操作次数超限被拒。
    rate_limit_cancels: usize,
    unknown_next_place: Option<String>,
    /// 还要以保证金不足拒绝几张会增加敞口的单（非只减仓）。
    reject_for_margin: usize,
    /// 下一张单照常挂上，但回执丢在路上（发送方只看到超时）。
    lose_next_place_response: Option<String>,
    /// 下一张立即成交否则撤销的单最多成交多少：用来构造「滑点上限内没成交完」（B38）。
    ioc_fill_cap: Option<Quantity>,
    /// 下一张只减仓标记等于它的单，请求还在路上：发送方收到结果未知，交易所还没收到
    /// （B39b、B39c）。
    hold_next_place: Option<bool>,
    /// 还在路上的请求，[`SimulatedExchange::deliver_held_places`] 时才到达交易所。
    held_places: Vec<OrderRequest>,
    /// 按 `cloid` 查询和挂单列表都暂时看不到的订单（交易所查询滞后）。
    hidden_from_queries: Vec<ClientOrderId>,
    /// 查标记价时，这个时钟往前走多久（行情响应很慢）。
    slow_mark_price: Option<(Arc<ManualClock>, Duration)>,
    /// 查成交流水时，这个时钟往前走多久（成交接口很慢）。
    slow_fills_since: Option<(Arc<ManualClock>, Duration)>,
    /// 每次查成交流水传进来的起点，按调用顺序。
    fills_since_starts: Vec<DateTime<Utc>>,
    /// 下一次查仓位时，把延迟的成交放出来。
    deliver_fills_on_next_position: bool,
    /// 每发一张单，这个时钟往前走多久（下单很慢）。
    slow_places: Option<(Arc<ManualClock>, Duration)>,
    /// 下一次查标记价失败的原因。行情只用于展示和判定，拿不到它不该挡住撤单。
    fail_next_mark_price: Option<String>,
    /// 还要让几次查仓位失败，以及失败的原因。
    fail_positions: usize,
    fail_positions_reason: String,
    /// 接下来几次查标记价依次拿到的结果：`Ok` 是这一次读到的价格（不改撮合用的行情），
    /// `Err` 是这一次失败的原因。用来构造「两次读数之间价格变了、或者第二次读不到」。
    scripted_mark_prices: std::collections::VecDeque<Result<Price, String>>,
    /// 接下来几次查挂单列表失败。撤哪几张单靠库里存着的 `cloid` 就够，问不到挂单
    /// 列表不该挡住撤单。
    fail_working_orders: usize,
    fail_working_orders_reason: String,
    /// 接下来几次查成交流水失败。
    fail_fills_since: usize,
    fail_fills_since_reason: String,
    unknown_cancels: usize,
    unknown_cancel_reason: String,
    reject_cancels: usize,
    reject_cancel_reason: String,
    /// 排队等着在接下来几次查仓位之前成交的挂单，先进先出。
    fill_before_next_position: VecDeque<(ClientOrderId, Quantity)>,
    /// 排队等着在接下来几次撤单之前成交的挂单，先进先出。
    fill_before_next_cancel: VecDeque<(ClientOrderId, Quantity, DateTime<Utc>)>,
    /// 已经过了查仓位、等着在下一次查成交时成交的那一笔。
    fill_after_next_position: Option<(ClientOrderId, Quantity)>,
    /// 接下来几次仓位查询报出的旧值（仓位接口滞后）。
    stale_positions: VecDeque<Position>,
    /// 接下来几次挂单列表和按 `cloid` 的查询都按这份旧的订单快照回答（订单接口滞后），
    /// 以及还剩几次。
    stale_orders: Option<(usize, Vec<SimOrder>)>,
    pending_fill: Option<(ClientOrderId, Quantity)>,
    place_count: usize,
    /// 收到的全部请求次数（见 [`SimulatedExchange::call_count`]）。
    call_count: usize,
    /// 可选的数据库句柄：设上之后每次下单都会先查一次这张单是不是已经落库，
    /// 用来在成功路径上验证「先写库、再发送」（第 9.1 节）。
    store: Option<Store>,
    /// 每次下单时查到的「这张单是不是已经落库」的结果，按顺序记录。
    stored_before_place: Vec<(ClientOrderId, bool)>,
}

#[derive(Clone)]
struct SimStop {
    request: StopRequest,
    exchange_order_id: String,
    cumulative_filled: Quantity,
    status: OrderStatus,
}

impl SimStop {
    fn is_crossed(&self, market_price: Price) -> bool {
        self.status == OrderStatus::Open
            && match self.request.side {
                Side::Sell => market_price <= self.request.trigger_price,
                Side::Buy => market_price >= self.request.trigger_price,
            }
    }

    fn view(&self) -> ExchangeOrder {
        ExchangeOrder {
            client_order_id: Some(self.request.client_order_id.clone()),
            exchange_order_id: self.exchange_order_id.clone(),
            side: self.request.side,
            limit_price: self.request.limit_price,
            quantity: self.request.quantity,
            cumulative_filled: self.cumulative_filled,
            reduce_only: true,
            status: self.status,
        }
    }
}

#[derive(Clone)]
struct SimOrder {
    request: OrderRequest,
    exchange_order_id: String,
    cumulative_filled: Quantity,
    status: OrderStatus,
}

impl SimOrder {
    fn remaining(&self) -> Quantity {
        self.request.quantity.saturating_sub(self.cumulative_filled)
    }

    /// 这张挂单现在最多能成交多少：剩余量，只减仓时还要受当前仓位限制。
    ///
    /// 只减仓不能把仓位做到反方向：卖出最多卖到零，买入最多买回到零。仓位已经被
    /// 别的方式（例如手动交易）平掉时，上限就是零。
    fn fillable(&self, position: Position) -> Quantity {
        let remaining = self.remaining();
        if !self.request.reduce_only {
            return remaining;
        }
        let capacity = match self.request.side {
            Side::Buy => Quantity::from_units(position.units().saturating_neg().max(0)),
            Side::Sell => Quantity::from_units(position.units().max(0)),
        };
        remaining.min(capacity)
    }

    fn is_working(&self) -> bool {
        matches!(
            self.status,
            OrderStatus::Open | OrderStatus::PartiallyFilled
        )
    }

    fn is_crossed(&self, market_price: Price) -> bool {
        self.is_working()
            && match self.request.side {
                Side::Buy => self.request.limit_price >= market_price,
                Side::Sell => self.request.limit_price <= market_price,
            }
    }

    fn view(&self) -> ExchangeOrder {
        ExchangeOrder {
            client_order_id: Some(self.request.client_order_id.clone()),
            exchange_order_id: self.exchange_order_id.clone(),
            side: self.request.side,
            limit_price: self.request.limit_price,
            quantity: self.request.quantity,
            cumulative_filled: self.cumulative_filled,
            reduce_only: self.request.reduce_only,
            status: self.status,
        }
    }
}

impl SimulatedExchange {
    pub fn new(instrument: Instrument, market_price: Price, now: DateTime<Utc>) -> Self {
        Self {
            instrument,
            state: Mutex::new(SimState {
                now,
                fee_rate: None,
                next_fill_price: None,
                market_price,
                position: Position::ZERO,
                orders: Vec::new(),
                stops: Vec::new(),
                reject_next_stop: None,
                fills: Vec::new(),
                delayed_fills: Vec::new(),
                delay_next_fills: 0,
                next_order_id: 0,
                next_trade_id: 0,
                rate_limit_next_place: None,
                rate_limit_cancels: 0,
                reject_next_place: None,
                unknown_next_place: None,
                lose_next_place_response: None,
                ioc_fill_cap: None,
                hold_next_place: None,
                held_places: Vec::new(),
                reject_for_margin: 0,
                hidden_from_queries: Vec::new(),
                slow_places: None,
                deliver_fills_on_next_position: false,
                slow_mark_price: None,
                slow_fills_since: None,
                fills_since_starts: Vec::new(),
                fail_next_mark_price: None,
                fail_positions: 0,
                fail_positions_reason: String::new(),
                scripted_mark_prices: std::collections::VecDeque::new(),
                fail_working_orders: 0,
                fail_working_orders_reason: String::new(),
                fail_fills_since: 0,
                fail_fills_since_reason: String::new(),
                unknown_cancels: 0,
                unknown_cancel_reason: String::new(),
                reject_cancels: 0,
                reject_cancel_reason: String::new(),
                fill_before_next_position: VecDeque::new(),
                fill_before_next_cancel: VecDeque::new(),
                fill_after_next_position: None,
                stale_positions: VecDeque::new(),
                stale_orders: None,
                pending_fill: None,
                place_count: 0,
                call_count: 0,
                store: None,
                stored_before_place: Vec::new(),
            }),
        }
    }

    /// 让这个模拟交易所在每次下单时先查一次数据库：这张单是不是已经写进去了。
    ///
    /// 用来在成功路径上验证「先写库、再发送」（第 9.1 节）：下单请求到达交易所时，
    /// 这张单必须已经在库里，否则进程在两者之间崩溃就会留下一张本地不知道的单。
    pub fn probe_stored_orders(&self, store: Store) {
        self.lock().store = Some(store);
    }

    /// 每次下单时查到的「已经落库」结果，按下单顺序排列。
    pub fn stored_before_place(&self) -> Vec<(ClientOrderId, bool)> {
        self.lock().stored_before_place.clone()
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    /// 设置最新成交价，并让穿过的挂单按各自限价全部成交。
    /// 下一笔成交按 `price` 成交，不按限价。
    pub fn set_next_fill_price(&self, price: Price) {
        self.lock().next_fill_price = Some(price);
    }

    /// 之后的成交按 `rate` 报手续费。
    pub fn set_fee_rate(&self, rate: Rate) {
        self.lock().fee_rate = Some(rate);
    }

    pub fn set_market_price(&self, price: Price, now: DateTime<Utc>) {
        let mut state = self.lock();
        state.now = now;
        state.market_price = price;
        match_crossed_orders(&mut state, now);
        trigger_crossed_stops(&mut state, now);
    }

    /// 交易所上的止损触发单（含已触发、已撤销的）。
    pub fn stops(&self) -> Vec<ExchangeOrder> {
        self.lock().stops.iter().map(SimStop::view).collect()
    }

    /// 还在等触发的止损单的触发价和数量。
    pub fn open_stops(&self) -> Vec<(Price, Quantity)> {
        self.lock()
            .stops
            .iter()
            .filter(|stop| stop.status == OrderStatus::Open)
            .map(|stop| (stop.request.trigger_price, stop.request.quantity))
            .collect()
    }

    /// 下一张止损单以 `reason` 被拒。
    pub fn reject_next_stop(&self, reason: &str) {
        self.lock().reject_next_stop = Some(reason.to_string());
    }

    /// 直接设定仓位：用来构造「账户上本来就有仓位」的场景。
    ///
    /// 真实账户上的仓位可能来自手动交易，也可能来自别的策略；模拟交易所平时只靠成交
    /// 累积，这里给测试一个直接的入口。
    pub fn set_position(&self, position: Position) {
        self.lock().position = position;
    }

    /// 强制让一张挂单成交 `quantity`（超过剩余量时按剩余量成交），返回实际成交量。
    ///
    /// 用来构造部分成交、以及离线期间发生的成交。
    pub fn fill(
        &self,
        client_order_id: &ClientOrderId,
        quantity: Quantity,
        now: DateTime<Utc>,
    ) -> Result<Quantity, ExchangeError> {
        let mut state = self.lock();
        state.now = now;
        let Some(index) = state
            .orders
            .iter()
            .position(|order| &order.request.client_order_id == client_order_id)
        else {
            return Err(ExchangeError::UnknownOrder {
                client_order_id: client_order_id.clone(),
            });
        };
        if !state.orders[index].is_working() {
            return Err(ExchangeError::Rejected {
                reason: format!("order `{client_order_id}` is not working"),
            });
        }
        let quantity = quantity.min(state.orders[index].remaining());
        Ok(record_fill(&mut state, index, quantity, now))
    }

    /// 投递一笔成交：按累计成交量的差额更新挂单和仓位，并追加一条成交记录。
    ///
    /// 时间戳可以早于上一次查询的位置，用来构造迟到的成交。成交记录里的累计量
    /// 原样保留传进来的值：交易所回报什么就返回什么，重复投递和累计量回退都要能构造。
    /// 挂单累计量不增加时（重复或回退）不改变挂单和仓位，只多一条成交记录。
    pub fn inject_fill(
        &self,
        trade_id: &str,
        client_order_id: &ClientOrderId,
        quantity: Quantity,
        cumulative_quantity: Quantity,
        filled_at: DateTime<Utc>,
    ) -> Result<(), ExchangeError> {
        let mut state = self.lock();
        let Some(index) = state
            .orders
            .iter()
            .position(|order| &order.request.client_order_id == client_order_id)
        else {
            return Err(ExchangeError::UnknownOrder {
                client_order_id: client_order_id.clone(),
            });
        };
        let (price, side) = {
            let order = &state.orders[index];
            (order.request.limit_price, order.request.side)
        };
        let delta = {
            let order = &mut state.orders[index];
            let cumulative = cumulative_quantity
                .min(order.request.quantity)
                .max(order.cumulative_filled);
            let delta = cumulative.saturating_sub(order.cumulative_filled);
            order.cumulative_filled = cumulative;
            if !delta.is_zero() {
                order.status = if order.remaining().is_zero() {
                    OrderStatus::Filled
                } else {
                    OrderStatus::PartiallyFilled
                };
            }
            delta
        };
        let signed = match side {
            Side::Buy => delta.units(),
            Side::Sell => -delta.units(),
        };
        state.position = Position::from_units(state.position.units() + signed);
        state.next_trade_id += 1;
        push_fill(
            &mut state,
            ExchangeFill {
                trade_id: trade_id.to_string(),
                client_order_id: client_order_id.clone(),
                price,
                quantity,
                cumulative_quantity,
                filled_at,
                fee: None,
            },
        );
        Ok(())
    }

    /// 投递一笔不属于本策略订单的成交（例如用户在同一账户里手动交易）。
    ///
    /// 只追加成交记录，不改变挂单和仓位。
    pub fn inject_foreign_fill(
        &self,
        trade_id: &str,
        client_order_id: &ClientOrderId,
        price: Price,
        quantity: Quantity,
        filled_at: DateTime<Utc>,
    ) {
        let mut state = self.lock();
        state.next_trade_id += 1;
        push_fill(
            &mut state,
            ExchangeFill {
                trade_id: trade_id.to_string(),
                client_order_id: client_order_id.clone(),
                price,
                quantity,
                cumulative_quantity: quantity,
                filled_at,
                fee: None,
            },
        );
    }

    /// 让接下来 `count` 笔成交暂时不出现在按时间查询的结果里（成交查询有延迟）。
    ///
    /// 成交本身照常改变挂单累计量和仓位，只是查不到；用
    /// [`SimulatedExchange::deliver_delayed_fills`] 让它们出现。
    pub fn delay_next_fills(&self, count: usize) {
        self.lock().delay_next_fills = count;
    }

    /// 下一次查仓位时把延迟的成交放出来：用来构造「这一步前半段没查到、仓位核对时才到」。
    pub fn deliver_delayed_fills_on_next_position(&self) {
        self.lock().deliver_fills_on_next_position = true;
    }

    /// 最近一次查成交流水传进来的起点；还没查过时是 `None`。
    pub fn last_fills_since(&self) -> Option<DateTime<Utc>> {
        self.lock().fills_since_starts.last().copied()
    }

    /// 延迟的成交现在可以查到了。
    pub fn deliver_delayed_fills(&self) {
        let mut state = self.lock();
        let mut delayed = std::mem::take(&mut state.delayed_fills);
        state.fills.append(&mut delayed);
    }

    /// 让接下来 `count` 次查询仓位之前各成交一笔。
    ///
    /// 用来构造「查成交」和「查仓位」之间刚好有成交的情形；连续排两笔就能构造
    /// 两次查询之间各成交一笔。
    pub fn fill_before_next_position(
        &self,
        count: usize,
        client_order_id: ClientOrderId,
        quantity: Quantity,
    ) {
        let mut state = self.lock();
        for _ in 0..count {
            state
                .fill_before_next_position
                .push_back((client_order_id.clone(), quantity));
        }
    }

    /// 让接下来 `count` 次撤单之前各成交一笔，成交时间用 `at`。
    ///
    /// 用来构造「撤单请求在路上时成交了一部分」的情形。
    pub fn fill_before_next_cancel(
        &self,
        count: usize,
        client_order_id: ClientOrderId,
        quantity: Quantity,
        at: DateTime<Utc>,
    ) {
        let mut state = self.lock();
        for _ in 0..count {
            state
                .fill_before_next_cancel
                .push_back((client_order_id.clone(), quantity, at));
        }
    }

    /// 让下一次查询仓位之后、下一次查询成交之前成交一笔。
    ///
    /// 用来构造「查完仓位、还没补拉成交时又成交了一笔」的情形。
    pub fn fill_after_next_position(&self, client_order_id: ClientOrderId, quantity: Quantity) {
        self.lock().fill_after_next_position = Some((client_order_id, quantity));
    }

    /// 让接下来 `count` 次仓位查询都报 `position`，而不是真实仓位（仓位接口滞后）。
    ///
    /// 成交和挂单状态照常变化，只有仓位查询停在旧值上。
    pub fn report_stale_positions(&self, count: usize, position: Position) {
        let mut state = self.lock();
        for _ in 0..count {
            state.stale_positions.push_back(position);
        }
    }

    /// 让接下来 `count` 次挂单列表或按 `cloid` 的查询都按此刻的订单快照回答（订单接口滞后）。
    ///
    /// 之后的成交照常改变仓位和成交流水，只有订单查询还停在成交之前：用来构造「仓位接口
    /// 先变、挂单和订单状态过几秒才跟上」（正式网实测）。
    pub fn lag_order_views(&self, count: usize) {
        let mut state = self.lock();
        let snapshot = state.orders.clone();
        state.stale_orders = Some((count, snapshot));
    }

    /// 用户在交易所手动交易，直接改变仓位，不产生属于本策略的订单。
    pub fn manual_trade(&self, quantity: Quantity) {
        let mut state = self.lock();
        state.position = Position::from_units(state.position.units() + quantity.units());
    }

    /// 交易所自己把一张挂单撤掉（自成交保护、只减仓冲突、账户被强平这类原因），
    /// 本系统没有发撤单请求。已经成交的部分留在累计成交量里。
    ///
    /// 用来构造「订单离开了挂单列表，但结局是撤销而不是成交」的情形。
    /// 返回这张单是否确实被撤掉了。
    pub fn cancel_externally(&self, client_order_id: &ClientOrderId) -> bool {
        let mut state = self.lock();
        let Some(order) = state
            .orders
            .iter_mut()
            .find(|order| &order.request.client_order_id == client_order_id)
        else {
            return false;
        };
        if !order.is_working() {
            return false;
        }
        order.status = OrderStatus::Canceled;
        true
    }

    /// 让下一次下单被拒绝。
    pub fn reject_next_place(&self, reason: &str) {
        self.lock().reject_next_place = Some(reason.to_string());
    }

    /// 让接下来 `count` 次撤单以操作次数超限被拒（B30）。
    pub fn rate_limit_cancels(&self, count: usize) {
        self.lock().rate_limit_cancels = count;
    }

    /// 让下一次下单以操作次数超限被拒（B30）。
    pub fn rate_limit_next_place(&self, reason: &str) {
        self.lock().rate_limit_next_place = Some(reason.to_string());
    }

    /// 让下一次下单结果未知：交易所没有收到这张单，也没有明确拒绝。
    pub fn unknown_next_place(&self, reason: &str) {
        self.lock().unknown_next_place = Some(reason.to_string());
    }

    /// 让接下来 `count` 张会增加敞口的单（非只减仓）以保证金不足被拒；只减仓单照常挂。
    pub fn reject_for_margin(&self, count: usize) {
        self.lock().reject_for_margin = count;
    }

    /// 让下一张单照常挂上（可能当场成交），但发送方收到的是结果未知。
    ///
    /// 用来构造「请求落地了、回执丢了」：按 `cloid` 查询能查到它。
    pub fn lose_next_place_response(&self, reason: &str) {
        self.lock().lose_next_place_response = Some(reason.to_string());
    }

    /// 下一张增仓单（不是只减仓的单）的请求在路上耽搁：发送方收到结果未知，交易所这时
    /// 还不知道它，查不到、撤不掉，要等 [`SimulatedExchange::deliver_held_places`] 才到达。
    pub fn hold_next_increasing_place(&self) {
        self.lock().hold_next_place = Some(false);
    }

    /// 同上，耽搁的是下一张只减仓单。
    pub fn hold_next_reducing_place(&self) {
        self.lock().hold_next_place = Some(true);
    }

    /// 在路上的请求在 `now` 这一刻到达交易所：没过 `expiresAfter` 的照常挂上、穿过行情的
    /// 当场成交；过了的被拒绝，没有任何影响（第 9.1 节）。
    pub fn deliver_held_places(&self, now: DateTime<Utc>) {
        let mut state = self.lock();
        state.now = now;
        for request in std::mem::take(&mut state.held_places) {
            if request.expires_at > now {
                // 到达之后的结局只能靠查询知道：发送方早就收到了「结果未知」。
                let _ = admit(&mut state, &request);
            }
        }
    }

    /// 下一张立即成交否则撤销的单最多成交 `quantity`，剩下的撤销（B38）。
    pub fn cap_next_ioc_fill(&self, quantity: Quantity) {
        self.lock().ioc_fill_cap = Some(quantity);
    }

    /// 让这张单从按 `cloid` 查询和挂单列表里消失，成交和仓位照常变化。
    ///
    /// 用来构造「交易所查不到它，可它其实落过地」。
    pub fn hide_from_queries(&self, client_order_id: &ClientOrderId) {
        self.lock()
            .hidden_from_queries
            .push(client_order_id.clone());
    }

    /// 让被 [`SimulatedExchange::hide_from_queries`] 藏起来的订单重新查得到。
    pub fn show_in_queries(&self, client_order_id: &ClientOrderId) {
        self.lock()
            .hidden_from_queries
            .retain(|hidden| hidden != client_order_id);
    }

    /// 每查一次标记价，让 `clock` 往前走 `by`：用来构造「行情响应在路上耽搁了」。
    pub fn slow_mark_price(&self, clock: Arc<ManualClock>, by: Duration) {
        self.lock().slow_mark_price = Some((clock, by));
    }

    /// 每查一次成交流水，让 `clock` 往前走 `by`：用来构造「读取阶段很慢，步首读的保证金
    /// 读数到发单时已经旧了」。
    pub fn slow_fills_since(&self, clock: Arc<ManualClock>, by: Duration) {
        self.lock().slow_fills_since = Some((clock, by));
    }

    /// 每发一张单，让 `clock` 往前走 `by`：用来构造「一步还没发完，行情已经旧了」。
    pub fn slow_places(&self, clock: Arc<ManualClock>, by: Duration) {
        self.lock().slow_places = Some((clock, by));
    }

    /// 让下一次查标记价失败：行情源暂时不可用。
    ///
    /// 行情、可用资金、仓位都是展示用的读数，拿不到它们不该影响撤单。
    pub fn fail_next_mark_price(&self, reason: &str) {
        self.lock().fail_next_mark_price = Some(reason.to_string());
    }

    /// 让接下来 `count` 次查仓位失败：交易所的账户接口暂时不可用。
    pub fn fail_positions(&self, count: usize, reason: &str) {
        let mut state = self.lock();
        state.fail_positions = count;
        state.fail_positions_reason = reason.to_string();
    }

    /// 接下来几次查标记价依次返回 `results`，用完之后照常返回当前行情。
    pub fn script_mark_prices(&self, results: Vec<Result<Price, &str>>) {
        self.lock().scripted_mark_prices = results
            .into_iter()
            .map(|result| result.map_err(str::to_string))
            .collect();
    }

    /// 让接下来 `count` 次查挂单列表失败：交易所的订单接口暂时不可用。
    ///
    /// 挂单列表是用来核对订单的，撤单本身只按库里存着的 `cloid` 发请求，问不到列表
    /// 不该让挂单留在交易所上。
    pub fn fail_working_orders(&self, count: usize, reason: &str) {
        let mut state = self.lock();
        state.fail_working_orders = count;
        state.fail_working_orders_reason = reason.to_string();
    }

    /// 让接下来 `count` 次查成交流水失败。
    pub fn fail_fills_since(&self, count: usize, reason: &str) {
        let mut state = self.lock();
        state.fail_fills_since = count;
        state.fail_fills_since_reason = reason.to_string();
    }

    /// 让下一次撤单结果未知：请求超时，订单还在交易所。
    pub fn unknown_next_cancel(&self, reason: &str) {
        self.unknown_next_cancels(1, reason);
    }

    /// 让接下来 `count` 次撤单都结果未知。
    ///
    /// 撤单回执丢在路上的时候，一次停止要发出去的是全部在场订单的撤销请求，丢的是
    /// 这一批回执，不是其中一张。
    pub fn unknown_next_cancels(&self, count: usize, reason: &str) {
        let mut state = self.lock();
        state.unknown_cancels = count;
        state.unknown_cancel_reason = reason.to_string();
    }

    /// 让接下来 `count` 次撤单被明确拒绝：交易所拒收，订单还在场。
    pub fn reject_next_cancels(&self, count: usize, reason: &str) {
        let mut state = self.lock();
        state.reject_cancels = count;
        state.reject_cancel_reason = reason.to_string();
    }

    pub fn current_position(&self) -> Position {
        self.lock().position
    }

    pub fn orders(&self) -> Vec<ExchangeOrder> {
        self.lock().orders.iter().map(SimOrder::view).collect()
    }

    pub fn fills(&self) -> Vec<ExchangeFill> {
        self.lock().fills.clone()
    }

    /// 收到的下单请求次数（含被拒绝的）。
    pub fn place_count(&self) -> usize {
        self.lock().place_count
    }

    /// 收到的全部请求次数，含下单、撤单、查挂单、查订单状态、查成交、查仓位、查标记价。
    ///
    /// 用来验证「读详情不碰交易所」这类要求：只看下单次数不够——读一次详情可能不下单，
    /// 却问了一遍行情和仓位。
    pub fn call_count(&self) -> usize {
        self.lock().call_count
    }

    /// 把请求计数清零，方便只统计之后的一段。
    pub fn reset_call_count(&self) {
        self.lock().call_count = 0;
    }

    fn lock(&self) -> MutexGuard<'_, SimState> {
        self.state.lock().expect("simulated exchange lock poisoned")
    }
}

#[async_trait]
impl Exchange for SimulatedExchange {
    async fn place_order(&self, request: &OrderRequest) -> Result<OrderReceipt, ExchangeError> {
        // 「先写库、再发送」：请求到达交易所时这张单必须已经在库里。
        let store = self.lock().store.clone();
        if let Some(store) = store {
            let stored = store
                .has_order(&request.client_order_id)
                .expect("the store must be readable while probing placements");
            assert!(
                stored,
                "order `{}` reached the exchange before it was stored",
                request.client_order_id
            );
            self.lock()
                .stored_before_place
                .push((request.client_order_id.clone(), stored));
        }
        let mut state = self.lock();
        state.place_count += 1;
        state.call_count += 1;
        if let Some((clock, by)) = &state.slow_places {
            clock.advance(*by);
        }
        if request.instrument != self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!(
                    "instrument `{}` does not belong to this exchange",
                    request.instrument
                ),
            });
        }
        if let Some(reason) = state.reject_next_place.take() {
            return Err(ExchangeError::Rejected { reason });
        }
        if let Some(reason) = state.rate_limit_next_place.take() {
            return Err(ExchangeError::RateLimited { reason });
        }
        if let Some(reason) = state.unknown_next_place.take() {
            return Err(ExchangeError::Unknown { reason });
        }
        if !request.reduce_only && state.reject_for_margin > 0 {
            state.reject_for_margin -= 1;
            return Err(ExchangeError::InsufficientMargin {
                reason: "Insufficient margin to place order.".to_string(),
            });
        }
        if request.expires_at <= state.now {
            return Err(ExchangeError::Rejected {
                reason: format!("order expired at {}", request.expires_at),
            });
        }
        if state
            .orders
            .iter()
            .map(|order| &order.request)
            .chain(&state.held_places)
            .any(|held| held.client_order_id == request.client_order_id)
        {
            return Err(ExchangeError::Rejected {
                reason: format!(
                    "client order id `{}` was already used",
                    request.client_order_id
                ),
            });
        }

        if state.hold_next_place == Some(request.reduce_only) {
            state.hold_next_place = None;
            state.held_places.push(request.clone());
            return Err(ExchangeError::Unknown {
                reason: "the request is still in flight".to_string(),
            });
        }

        let index = admit(&mut state, request)?;
        if let Some(reason) = state.lose_next_place_response.take() {
            return Err(ExchangeError::Unknown { reason });
        }
        let order = &state.orders[index];
        Ok(OrderReceipt {
            exchange_order_id: order.exchange_order_id.clone(),
            cumulative_filled: order.cumulative_filled,
            status: order.status,
        })
    }

    async fn place_stop(&self, request: &StopRequest) -> Result<OrderReceipt, ExchangeError> {
        let mut state = self.lock();
        state.place_count += 1;
        state.call_count += 1;
        if request.instrument != self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!(
                    "instrument `{}` does not belong to this exchange",
                    request.instrument
                ),
            });
        }
        if let Some(reason) = state.reject_next_stop.take() {
            return Err(ExchangeError::Rejected { reason });
        }
        if request.expires_at <= state.now {
            return Err(ExchangeError::Rejected {
                reason: format!("order expired at {}", request.expires_at),
            });
        }
        state.next_order_id += 1;
        let exchange_order_id = format!("sim-stop-{}", state.next_order_id);
        state.stops.push(SimStop {
            request: request.clone(),
            exchange_order_id: exchange_order_id.clone(),
            cumulative_filled: Quantity::ZERO,
            status: OrderStatus::Open,
        });
        let now = state.now;
        trigger_crossed_stops(&mut state, now);
        Ok(OrderReceipt {
            exchange_order_id,
            cumulative_filled: Quantity::ZERO,
            status: OrderStatus::Open,
        })
    }

    async fn cancel_order(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<(), ExchangeError> {
        let mut state = self.lock();
        state.call_count += 1;
        if let Some(stop) = state
            .stops
            .iter_mut()
            .find(|stop| &stop.request.client_order_id == client_order_id)
        {
            if stop.status != OrderStatus::Open {
                return Err(ExchangeError::UnknownOrder {
                    client_order_id: client_order_id.clone(),
                });
            }
            stop.status = OrderStatus::Canceled;
            return Ok(());
        }
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        // 撤单请求在路上时挂单还在场，可能刚好成交了一部分。
        if let Some((client_order_id, quantity, at)) = state.fill_before_next_cancel.pop_front() {
            state.now = at;
            fill_order(&mut state, &client_order_id, quantity);
        }
        if state.unknown_cancels > 0 {
            state.unknown_cancels -= 1;
            let reason = state.unknown_cancel_reason.clone();
            return Err(ExchangeError::Unknown { reason });
        }
        if state.rate_limit_cancels > 0 {
            state.rate_limit_cancels -= 1;
            return Err(ExchangeError::RateLimited {
                reason: "Too many cumulative requests sent".to_string(),
            });
        }
        if state.reject_cancels > 0 {
            state.reject_cancels -= 1;
            let reason = state.reject_cancel_reason.clone();
            return Err(ExchangeError::Rejected { reason });
        }
        let Some(order) = state
            .orders
            .iter_mut()
            .find(|order| &order.request.client_order_id == client_order_id)
        else {
            return Err(ExchangeError::UnknownOrder {
                client_order_id: client_order_id.clone(),
            });
        };
        if order.is_working() {
            order.status = OrderStatus::Canceled;
        }
        Ok(())
    }

    async fn working_orders(
        &self,
        instrument: &Instrument,
    ) -> Result<Vec<ExchangeOrder>, ExchangeError> {
        let mut state = self.lock();
        state.call_count += 1;
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        if state.fail_working_orders > 0 {
            state.fail_working_orders -= 1;
            let reason = state.fail_working_orders_reason.clone();
            return Err(ExchangeError::Unavailable { reason });
        }
        let orders = take_stale_orders(&mut state).unwrap_or_else(|| state.orders.clone());
        Ok(orders
            .iter()
            .filter(|order| order.is_working())
            .filter(|order| {
                !state
                    .hidden_from_queries
                    .contains(&order.request.client_order_id)
            })
            .map(SimOrder::view)
            .collect())
    }

    async fn order_status(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<OrderLookup, ExchangeError> {
        let mut state = self.lock();
        state.call_count += 1;
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        if state.hidden_from_queries.contains(client_order_id) {
            return Ok(OrderLookup::Unknown);
        }
        if let Some(stop) = state
            .stops
            .iter()
            .find(|stop| &stop.request.client_order_id == client_order_id)
        {
            return Ok(OrderLookup::Found(stop.view()));
        }
        let orders = take_stale_orders(&mut state).unwrap_or_else(|| state.orders.clone());
        Ok(
            match orders
                .iter()
                .find(|order| &order.request.client_order_id == client_order_id)
            {
                Some(order) => OrderLookup::Found(order.view()),
                None => OrderLookup::Unknown,
            },
        )
    }

    async fn fills_since(
        &self,
        instrument: &Instrument,
        since: DateTime<Utc>,
    ) -> Result<Vec<ExchangeFill>, ExchangeError> {
        let mut state = self.lock();
        state.call_count += 1;
        state.fills_since_starts.push(since);
        if let Some((clock, by)) = &state.slow_fills_since {
            clock.advance(*by);
        }
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        if let Some((client_order_id, quantity)) = state.pending_fill.take() {
            fill_order(&mut state, &client_order_id, quantity);
        }
        if state.fail_fills_since > 0 {
            state.fail_fills_since -= 1;
            let reason = state.fail_fills_since_reason.clone();
            return Err(ExchangeError::Unavailable { reason });
        }
        Ok(state
            .fills
            .iter()
            .filter(|fill| fill.filled_at >= since)
            .cloned()
            .collect())
    }

    async fn position(&self, instrument: &Instrument) -> Result<Position, ExchangeError> {
        let mut state = self.lock();
        state.call_count += 1;
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        if state.fail_positions > 0 {
            state.fail_positions -= 1;
            return Err(ExchangeError::Unavailable {
                reason: state.fail_positions_reason.clone(),
            });
        }
        if let Some((client_order_id, quantity)) = state.fill_before_next_position.pop_front() {
            fill_order(&mut state, &client_order_id, quantity);
        }
        // 这一笔要等到下一次查成交时才成交：查到的仓位里还没有它。
        state.pending_fill = state.fill_after_next_position.take();
        if std::mem::take(&mut state.deliver_fills_on_next_position) {
            let mut delayed = std::mem::take(&mut state.delayed_fills);
            state.fills.append(&mut delayed);
        }
        if let Some(stale) = state.stale_positions.pop_front() {
            return Ok(stale);
        }
        Ok(state.position)
    }

    async fn mark_price(&self, instrument: &Instrument) -> Result<Price, ExchangeError> {
        let mut state = self.lock();
        state.call_count += 1;
        if let Some((clock, by)) = &state.slow_mark_price {
            clock.advance(*by);
        }
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        if let Some(reason) = state.fail_next_mark_price.take() {
            return Err(ExchangeError::Unavailable { reason });
        }
        if let Some(scripted) = state.scripted_mark_prices.pop_front() {
            return scripted.map_err(|reason| ExchangeError::Unavailable { reason });
        }
        Ok(state.market_price)
    }
}

/// 让所有穿过最新价的挂单按各自限价全部成交。
/// 一张单落到交易所：挂上，穿过行情的当场成交；立即成交否则撤销的单剩下的撤销，
/// 一点都没成交时和 Hyperliquid 一样整张拒绝。返回它在挂单表里的下标。
fn admit(state: &mut SimState, request: &OrderRequest) -> Result<usize, ExchangeError> {
    state.next_order_id += 1;
    let order = SimOrder {
        request: request.clone(),
        exchange_order_id: format!("sim-order-{}", state.next_order_id),
        cumulative_filled: Quantity::ZERO,
        status: OrderStatus::Open,
    };
    state.orders.push(order);
    let index = state.orders.len() - 1;
    let immediate = request.time_in_force == TimeInForce::ImmediateOrCancel;
    if state.orders[index].is_crossed(state.market_price) {
        let mut quantity = state.orders[index].remaining();
        if immediate && let Some(cap) = state.ioc_fill_cap.take() {
            quantity = quantity.min(cap);
        }
        let now = state.now;
        record_fill(state, index, quantity, now);
    }
    if immediate && state.orders[index].is_working() {
        state.orders[index].status = OrderStatus::Canceled;
        if state.orders[index].cumulative_filled.is_zero() {
            return Err(ExchangeError::Rejected {
                reason: "Order could not immediately match against any resting orders.".to_string(),
            });
        }
    }
    Ok(index)
}

fn match_crossed_orders(state: &mut SimState, now: DateTime<Utc>) {
    loop {
        let market_price = state.market_price;
        let position = state.position;
        // 只减仓订单在仓位已经平掉时上限为零：必须在这里跳过，否则它一直算「穿过」
        // 却成交不了，这个循环不会结束。
        let Some(index) = state.orders.iter().position(|order| {
            order.is_crossed(market_price) && !order.fillable(position).is_zero()
        }) else {
            return;
        };
        let quantity = state.orders[index].fillable(position);
        record_fill(state, index, quantity, now);
    }
}

/// 标记价越过触发价的止损单：按标记价成交，只减仓，最多减到零；没有可减的仓位时
/// 和 Hyperliquid 一样撤销。
fn trigger_crossed_stops(state: &mut SimState, now: DateTime<Utc>) {
    for index in 0..state.stops.len() {
        if !state.stops[index].is_crossed(state.market_price) {
            continue;
        }
        let (side, quantity, client_order_id) = {
            let stop = &state.stops[index];
            (
                stop.request.side,
                stop.request.quantity,
                stop.request.client_order_id.clone(),
            )
        };
        let capacity = match side {
            Side::Buy => Quantity::from_units(state.position.units().saturating_neg().max(0)),
            Side::Sell => Quantity::from_units(state.position.units().max(0)),
        };
        let applied = quantity.min(capacity);
        let stop = &mut state.stops[index];
        if applied.is_zero() {
            stop.status = OrderStatus::Canceled;
            continue;
        }
        stop.cumulative_filled = applied;
        stop.status = if applied == quantity {
            OrderStatus::Filled
        } else {
            OrderStatus::Canceled
        };
        let signed = match side {
            Side::Buy => applied.units(),
            Side::Sell => -applied.units(),
        };
        state.position = Position::from_units(state.position.units() + signed);
        state.next_trade_id += 1;
        let fill = ExchangeFill {
            trade_id: format!("trade-{}", state.next_trade_id),
            client_order_id,
            price: state.market_price,
            quantity: applied,
            cumulative_quantity: applied,
            filled_at: now,
            fee: None,
        };
        push_fill(state, fill);
    }
}

/// 让一张挂单按自己的限价成交 `quantity`（超过剩余量时按剩余量成交）。
///
/// 成交时间戳用模拟交易所当前的时钟；测试要走远时间时先用
/// [`SimulatedExchange::fill`] 或 [`SimulatedExchange::set_market_price`] 把时钟推过去，
/// 否则这笔成交会落在按时间查询的范围之外。
fn fill_order(state: &mut SimState, client_order_id: &ClientOrderId, quantity: Quantity) {
    let Some(index) = state
        .orders
        .iter()
        .position(|order| &order.request.client_order_id == client_order_id)
    else {
        return;
    };
    let quantity = quantity.min(state.orders[index].remaining());
    let now = state.now;
    record_fill(state, index, quantity, now);
}

/// 记一笔成交并更新仓位。
///
/// 只减仓订单的成交数量先按当前仓位收窄：收窄到零时不产生成交，挂单保持在场。
fn record_fill(
    state: &mut SimState,
    index: usize,
    quantity: Quantity,
    now: DateTime<Utc>,
) -> Quantity {
    let quantity = quantity.min(state.orders[index].fillable(state.position));
    if quantity.is_zero() {
        return Quantity::ZERO;
    }
    let (client_order_id, limit_price, side) = {
        let order = &state.orders[index];
        (
            order.request.client_order_id.clone(),
            order.request.limit_price,
            order.request.side,
        )
    };
    let (applied, cumulative) = {
        let order = &mut state.orders[index];
        let cumulative = order
            .cumulative_filled
            .saturating_add(quantity)
            .min(order.request.quantity);
        let applied = cumulative.saturating_sub(order.cumulative_filled);
        order.cumulative_filled = cumulative;
        order.status = if order.remaining().is_zero() {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        (applied, cumulative)
    };

    let signed = match side {
        Side::Buy => applied.units(),
        Side::Sell => -applied.units(),
    };
    state.position = Position::from_units(state.position.units() + signed);
    state.next_trade_id += 1;
    let price = state.next_fill_price.take().unwrap_or(limit_price);
    push_fill(
        state,
        ExchangeFill {
            trade_id: format!("trade-{}", state.next_trade_id),
            client_order_id,
            price,
            quantity: applied,
            cumulative_quantity: cumulative,
            filled_at: now,
            fee: None,
        },
    );
    applied
}

/// 订单查询还在滞后时，取这一次要回答的旧快照并把剩余次数减一；没在滞后时是 `None`。
fn take_stale_orders(state: &mut SimState) -> Option<Vec<SimOrder>> {
    let (remaining, snapshot) = state.stale_orders.as_mut()?;
    if *remaining == 0 {
        state.stale_orders = None;
        return None;
    }
    *remaining -= 1;
    let snapshot = snapshot.clone();
    if *remaining == 0 {
        state.stale_orders = None;
    }
    Some(snapshot)
}

/// 记一条成交。设了延迟时先放进「查不到」的那一堆。
fn push_fill(state: &mut SimState, mut fill: ExchangeFill) {
    if let Some(rate) = state.fee_rate {
        fill.fee = Some(Notional::of(fill.quantity, fill.price).mul_rate(rate));
    }
    if state.delay_next_fills > 0 {
        state.delay_next_fills -= 1;
        state.delayed_fills.push(fill);
    } else {
        state.fills.push(fill);
    }
}
