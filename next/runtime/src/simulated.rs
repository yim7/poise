//! 模拟交易所。
//!
//! 只用于测试：不连接任何外部服务，也不做订单之间的撮合。
//! 价格穿过某张挂单的限价时，这张挂单按自己的限价全部成交。
//!
//! 时间由测试通过 [`SimulatedExchange::set_market_price`] 和
//! [`SimulatedExchange::fill`] 推进；模拟交易所只用一个时间点判断下单是否已过期，
//! 不会自动撤销过期挂单（撤销由 runtime 负责）。

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{ClientOrderId, OrderStatus, Side};
use poise_strategy::units::{Position, Price, Quantity};

use crate::exchange::{
    Exchange, ExchangeError, ExchangeFill, ExchangeOrder, OrderLookup, OrderReceipt, OrderRequest,
};
use crate::store::Store;

pub struct SimulatedExchange {
    instrument: Instrument,
    state: Mutex<SimState>,
}

struct SimState {
    /// 模拟交易所看到的当前时间，只用于判断下单是否已过期。
    now: DateTime<Utc>,
    market_price: Price,
    position: Position,
    orders: Vec<SimOrder>,
    fills: Vec<ExchangeFill>,
    /// 暂时不出现在按时间查询结果里的成交（模拟成交查询有延迟）。
    delayed_fills: Vec<ExchangeFill>,
    delay_next_fills: usize,
    next_order_id: u64,
    next_trade_id: u64,
    reject_next_place: Option<String>,
    unknown_next_place: Option<String>,
    unknown_next_cancel: Option<String>,
    /// 排队等着在接下来几次查仓位之前成交的挂单，先进先出。
    fill_before_next_position: VecDeque<(ClientOrderId, Quantity)>,
    /// 排队等着在接下来几次撤单之前成交的挂单，先进先出。
    fill_before_next_cancel: VecDeque<(ClientOrderId, Quantity, DateTime<Utc>)>,
    /// 已经过了查仓位、等着在下一次查成交时成交的那一笔。
    fill_after_next_position: Option<(ClientOrderId, Quantity)>,
    pending_fill: Option<(ClientOrderId, Quantity)>,
    place_count: usize,
    /// 可选的数据库句柄：设上之后每次下单都会先查一次这张单是不是已经落库，
    /// 用来在成功路径上验证「先写库、再发送」（第 9.1 节）。
    store: Option<Store>,
    /// 每次下单时查到的「这张单是不是已经落库」的结果，按顺序记录。
    stored_before_place: Vec<(ClientOrderId, bool)>,
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
            client_order_id: self.request.client_order_id.clone(),
            exchange_order_id: self.exchange_order_id.clone(),
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
                market_price,
                position: Position::ZERO,
                orders: Vec::new(),
                fills: Vec::new(),
                delayed_fills: Vec::new(),
                delay_next_fills: 0,
                next_order_id: 0,
                next_trade_id: 0,
                reject_next_place: None,
                unknown_next_place: None,
                unknown_next_cancel: None,
                fill_before_next_position: VecDeque::new(),
                fill_before_next_cancel: VecDeque::new(),
                fill_after_next_position: None,
                pending_fill: None,
                place_count: 0,
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
    pub fn set_market_price(&self, price: Price, now: DateTime<Utc>) {
        let mut state = self.lock();
        state.now = now;
        state.market_price = price;
        match_crossed_orders(&mut state, now);
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

    /// 用户在交易所手动交易，直接改变仓位，不产生属于本策略的订单。
    pub fn manual_trade(&self, quantity: Quantity) {
        let mut state = self.lock();
        state.position = Position::from_units(state.position.units() + quantity.units());
    }

    /// 让下一次下单被拒绝。
    pub fn reject_next_place(&self, reason: &str) {
        self.lock().reject_next_place = Some(reason.to_string());
    }

    /// 让下一次下单结果未知：交易所没有收到这张单，也没有明确拒绝。
    pub fn unknown_next_place(&self, reason: &str) {
        self.lock().unknown_next_place = Some(reason.to_string());
    }

    /// 让下一次撤单结果未知：请求超时，订单还在交易所。
    pub fn unknown_next_cancel(&self, reason: &str) {
        self.lock().unknown_next_cancel = Some(reason.to_string());
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
        if let Some(reason) = state.unknown_next_place.take() {
            return Err(ExchangeError::Unknown { reason });
        }
        if request.expires_at <= state.now {
            return Err(ExchangeError::Rejected {
                reason: format!("order expired at {}", request.expires_at),
            });
        }
        if state
            .orders
            .iter()
            .any(|order| order.request.client_order_id == request.client_order_id)
        {
            return Err(ExchangeError::Rejected {
                reason: format!(
                    "client order id `{}` was already used",
                    request.client_order_id
                ),
            });
        }

        state.next_order_id += 1;
        let order = SimOrder {
            request: request.clone(),
            exchange_order_id: format!("sim-order-{}", state.next_order_id),
            cumulative_filled: Quantity::ZERO,
            status: OrderStatus::Open,
        };
        state.orders.push(order);
        let index = state.orders.len() - 1;
        if state.orders[index].is_crossed(state.market_price) {
            let quantity = state.orders[index].remaining();
            let now = state.now;
            record_fill(&mut state, index, quantity, now);
        }
        let order = &state.orders[index];
        Ok(OrderReceipt {
            exchange_order_id: order.exchange_order_id.clone(),
            cumulative_filled: order.cumulative_filled,
            status: order.status,
        })
    }

    async fn cancel_order(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<(), ExchangeError> {
        let mut state = self.lock();
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
        if let Some(reason) = state.unknown_next_cancel.take() {
            return Err(ExchangeError::Unknown { reason });
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
        let state = self.lock();
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        Ok(state
            .orders
            .iter()
            .filter(|order| order.is_working())
            .map(SimOrder::view)
            .collect())
    }

    async fn order_status(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<OrderLookup, ExchangeError> {
        let state = self.lock();
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        Ok(
            match state
                .orders
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
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        if let Some((client_order_id, quantity)) = state.pending_fill.take() {
            fill_order(&mut state, &client_order_id, quantity);
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
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        if let Some((client_order_id, quantity)) = state.fill_before_next_position.pop_front() {
            fill_order(&mut state, &client_order_id, quantity);
        }
        // 这一笔要等到下一次查成交时才成交：查到的仓位里还没有它。
        state.pending_fill = state.fill_after_next_position.take();
        Ok(state.position)
    }

    async fn mark_price(&self, instrument: &Instrument) -> Result<Price, ExchangeError> {
        let state = self.lock();
        if instrument != &self.instrument {
            return Err(ExchangeError::Rejected {
                reason: format!("instrument `{instrument}` does not belong to this exchange"),
            });
        }
        Ok(state.market_price)
    }
}

/// 让所有穿过最新价的挂单按各自限价全部成交。
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
    push_fill(
        state,
        ExchangeFill {
            trade_id: format!("trade-{}", state.next_trade_id),
            client_order_id,
            price: limit_price,
            quantity: applied,
            cumulative_quantity: cumulative,
            filled_at: now,
        },
    );
    applied
}

/// 记一条成交。设了延迟时先放进「查不到」的那一堆。
fn push_fill(state: &mut SimState, fill: ExchangeFill) {
    if state.delay_next_fills > 0 {
        state.delay_next_fills -= 1;
        state.delayed_fills.push(fill);
    } else {
        state.fills.push(fill);
    }
}
