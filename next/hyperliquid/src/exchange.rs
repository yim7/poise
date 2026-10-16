//! [`Exchange`] 的 Hyperliquid 实现。

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise_runtime::{
    Exchange, ExchangeError, ExchangeFill, ExchangeOrder, OrderLookup, OrderReceipt, OrderRequest,
};
use poise_strategy::instrument::Instrument;
use poise_strategy::order::{ClientOrderId, OrderStatus, Side};
use poise_strategy::units::{Position, Price, Quantity};
use serde_json::{Value, json};

use crate::actions::{
    CancelByCloid, CancelByCloidAction, ExchangeAction, LimitOrderType, OrderAction, OrderType,
};
use crate::client::Hyperliquid;
use crate::ids::{client_order_id_of, cloid_of};
use crate::models::{ClearinghouseState, FillResponse, OpenOrderResponse, OrderStatusResponse};

/// `userFillsByTime` 一页最多这么多条。
const FILLS_PAGE_SIZE: usize = 2_000;
/// 交易所只保留最近这么多笔成交；到了这个量就证明不了窗口是完整的。
const FILLS_RETENTION_LIMIT: usize = 10_000;

#[async_trait]
impl Exchange for Hyperliquid {
    async fn place_order(&self, request: &OrderRequest) -> Result<OrderReceipt, ExchangeError> {
        let asset = self.asset(&request.instrument).await?;
        let cloid = cloid_of(&request.client_order_id).map_err(rejected)?;
        // 报价和数量先按交易所的原始规则验一遍：不合规就本地拒绝，
        // 不静默改成别的价格或数量。
        asset.check_price(request.limit_price).map_err(rejected)?;
        asset.check_quantity(request.quantity).map_err(rejected)?;

        let action = ExchangeAction::Order(OrderAction {
            orders: vec![crate::actions::OrderRequest {
                asset: asset.index,
                is_buy: request.side == poise_strategy::order::Side::Buy,
                limit_px: request.limit_price.to_string(),
                sz: request.quantity.to_string(),
                reduce_only: request.reduce_only,
                order_type: OrderType::Limit(LimitOrderType { tif: "Gtc" }),
                cloid,
            }],
            grouping: "na",
        });
        // 交易所侧有效期：过期才送达的动作交易所会拒掉，这张单就不会生效。
        let response = self
            .exchange(
                &action,
                Some(request.expires_at.timestamp_millis().max(0) as u64),
            )
            .await?;
        order_receipt(&response)
    }

    async fn cancel_order(
        &self,
        instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<(), ExchangeError> {
        let asset = self.asset(instrument).await?;
        let cloid = cloid_of(client_order_id).map_err(rejected)?;
        let action = ExchangeAction::CancelByCloid(CancelByCloidAction {
            cancels: vec![CancelByCloid {
                asset: asset.index,
                cloid,
            }],
        });
        // 撤单不带有效期：官方 SDK 的 `cancelByCloid` 也没有这个参数。
        let response = self.exchange(&action, None).await?;
        cancel_outcome(&response, client_order_id)
    }

    async fn working_orders(
        &self,
        instrument: &Instrument,
    ) -> Result<Vec<ExchangeOrder>, ExchangeError> {
        let body = json!({
            "type": "openOrders",
            "user": self.user(),
        });
        let orders: Vec<OpenOrderResponse> = self.info(&body, "openOrders").await?;
        orders
            .into_iter()
            .filter(|order| order.coin == instrument.symbol())
            // 没有 `cloid` 的挂单不是本系统下的：照样列出，预览要给用户看（第 8 节第 4 条）；
            // 运行时按客户端订单 ID 对账，自然不会碰它。
            .map(open_order)
            .collect()
    }

    async fn order_status(
        &self,
        _instrument: &Instrument,
        client_order_id: &ClientOrderId,
    ) -> Result<OrderLookup, ExchangeError> {
        let body = json!({
            "type": "orderStatus",
            "user": self.user(),
            "oid": cloid_of(client_order_id).map_err(rejected)?,
        });
        let response: Value = self.info(&body, "orderStatus").await?;
        match response.get("status").and_then(Value::as_str) {
            // 交易所查不到这张单：可能没落地、可能早就撤了、也可能已经成交。
            Some("unknownOid") => Ok(OrderLookup::Unknown),
            Some("order") => {
                let order: OrderStatusResponse =
                    serde_json::from_value(response.get("order").cloned().unwrap_or(Value::Null))
                        .map_err(|error| ExchangeError::Unavailable {
                            reason: format!(
                                "cannot parse the `orderStatus` response as an order: {} error at line {} column {}",
                                crate::transport::category(&error),
                                error.line(),
                                error.column()
                            ),
                        })?;
                Ok(OrderLookup::Found(exchange_order(
                    order,
                    client_order_id.clone(),
                )?))
            }
            other => Err(ExchangeError::Unavailable {
                reason: format!(
                    "the exchange returned an unexpected `orderStatus` status: {}",
                    other.unwrap_or("<missing>")
                ),
            }),
        }
    }

    async fn fills_since(
        &self,
        instrument: &Instrument,
        since: DateTime<Utc>,
    ) -> Result<Vec<ExchangeFill>, ExchangeError> {
        let mut cursor = since.timestamp_millis();
        let mut unique: HashMap<u64, FillResponse> = HashMap::new();
        // 交易所的保留上限是按账户算的，不分标的，所以这里数的是返回的每一笔。
        let mut seen = 0_usize;
        loop {
            let body = json!({
                "type": "userFillsByTime",
                "user": self.user(),
                "startTime": cursor,
                // 按成交逐笔返回；聚合会丢掉单笔成交，累计量就没法算。
                "aggregateByTime": false,
            });
            let page: Vec<FillResponse> = self.info(&body, "userFillsByTime").await?;
            let count = page.len();
            seen += count;
            if seen >= FILLS_RETENTION_LIMIT {
                return Err(ExchangeError::Unknown {
                    reason: format!(
                        "this window holds {FILLS_RETENTION_LIMIT} fills or more, which is the exchange retention limit; the window cannot be proven complete"
                    ),
                });
            }
            let latest = page.last().map(|fill| fill.time);
            for fill in page {
                if fill.coin != instrument.symbol() {
                    continue;
                }
                match unique.get(&fill.tid) {
                    Some(existing) if existing != &fill => {
                        return Err(ExchangeError::Unknown {
                            reason: format!(
                                "the exchange returned two different fills with trade id {}",
                                fill.tid
                            ),
                        });
                    }
                    Some(_) => {}
                    None => {
                        unique.insert(fill.tid, fill);
                    }
                }
            }
            if count < FILLS_PAGE_SIZE {
                break;
            }
            // 满页：从最后一条的时间继续（`startTime` 含边界，重复的一条会被 tid 去重）。
            let latest = latest.ok_or_else(|| ExchangeError::Unknown {
                reason: "the exchange returned a full page of fills without timestamps".to_string(),
            })?;
            if latest <= cursor {
                return Err(ExchangeError::Unknown {
                    reason: format!(
                        "paging through fills made no progress at {cursor}; the window cannot be proven complete"
                    ),
                });
            }
            cursor = latest;
        }

        Ok(ordered_fills(unique))
    }

    async fn position(&self, instrument: &Instrument) -> Result<Position, ExchangeError> {
        let body = json!({
            "type": "clearinghouseState",
            "user": self.user(),
        });
        let state: ClearinghouseState = self.info(&body, "clearinghouseState").await?;
        let szi = state
            .asset_positions
            .iter()
            .find(|position| position.position.coin == instrument.symbol())
            .map(|position| position.position.szi.as_str())
            .unwrap_or("0");
        Position::parse(szi).map_err(|_| ExchangeError::Unavailable {
            reason: format!(
                "the exchange returned an invalid position for `{}`",
                instrument.symbol()
            ),
        })
    }

    async fn mark_price(&self, instrument: &Instrument) -> Result<Price, ExchangeError> {
        Hyperliquid::mark_price(self, instrument).await
    }
}

/// 交易所的订单状态变成领域状态。看不懂的状态按「结果未知」处理：它仍然算在场。
///
/// 撤销的取值不止 `canceled` 一个：自成交保护、只减仓冲突、持仓上限、被强平、
/// 标的退市等等各有各的终态，但它们都是「这张单结束了」。认不出来就会一直当成
/// 在场订单，预留永远不释放，格子也就永远补不上新单（设计附录 A）。
///
/// 这些取值按交易所文档逐个列出，不做「凡是以 `Canceled` 结尾都算撤销」的推断：
/// 把一张其实还在场的单误判成已结束，会让运行时释放预留、为同一格再下一张单，
/// 结果是重复买入。宁可停在「结果未知」上不动。
fn order_status_of(status: &str) -> OrderStatus {
    match status {
        "open" | "triggered" => OrderStatus::Open,
        "filled" => OrderStatus::Filled,
        // 撤销类终态：订单已经结束，之前成交的部分仍然算数（累计成交量由调用方带上）。
        "canceled"
        | "marginCanceled"
        | "selfTradeCanceled"
        | "reduceOnlyCanceled"
        | "openInterestCapCanceled"
        | "siblingFilledCanceled"
        | "delistedCanceled"
        | "liquidatedCanceled"
        | "scheduledCancel"
        | "vaultWithdrawalCanceled" => OrderStatus::Canceled,
        // 拒绝：这张单没有成立，也就没有成交。
        "rejected" => OrderStatus::Rejected,
        // 交易所对拒绝还给出带原因的变体（设计文档提到的 `minTradeNtlRejected` 就是其中之一）。
        other if other.ends_with("Rejected") => OrderStatus::Rejected,
        _ => OrderStatus::Unknown,
    }
}

/// 买卖方向：Hyperliquid 用 `B` 表示买、`A` 表示卖。
fn side_of(side: &str) -> Result<Side, ExchangeError> {
    match side {
        "B" => Ok(Side::Buy),
        "A" => Ok(Side::Sell),
        _ => Err(malformed("a side")),
    }
}

/// 累计成交量 = 原始数量 − 剩余数量。
///
/// 交易所不给原始数量时只能按 0 算：宁可少报（仓位核对会兜住），也不猜。
fn cumulative_filled(original: Option<&str>, remaining: &str) -> Result<Quantity, ExchangeError> {
    let remaining = Quantity::parse(remaining).map_err(|_| malformed("a size"))?;
    let Some(original) = original else {
        return Ok(Quantity::ZERO);
    };
    let original = Quantity::parse(original).map_err(|_| malformed("an original size"))?;
    Ok(original.saturating_sub(remaining))
}

fn exchange_order(
    order: OrderStatusResponse,
    client_order_id: ClientOrderId,
) -> Result<ExchangeOrder, ExchangeError> {
    let data = order.order;
    Ok(ExchangeOrder {
        client_order_id: Some(client_order_id),
        exchange_order_id: data.oid.to_string(),
        side: side_of(&data.side)?,
        limit_price: Price::parse(&data.limit_px).map_err(|_| malformed("a limit price"))?,
        quantity: Quantity::parse(data.orig_sz.as_deref().unwrap_or(&data.sz))
            .map_err(|_| malformed("a size"))?,
        cumulative_filled: cumulative_filled(data.orig_sz.as_deref(), &data.sz)?,
        reduce_only: data.reduce_only.unwrap_or(false),
        status: order_status_of(&order.status),
    })
}

fn open_order(order: OpenOrderResponse) -> Result<ExchangeOrder, ExchangeError> {
    let client_order_id = match order.cloid.as_deref() {
        Some(cloid) => {
            Some(client_order_id_of(cloid).ok_or_else(|| malformed("a client order id"))?)
        }
        None => None,
    };
    Ok(ExchangeOrder {
        client_order_id,
        exchange_order_id: order.oid.to_string(),
        side: side_of(&order.side)?,
        limit_price: Price::parse(&order.limit_px).map_err(|_| malformed("a limit price"))?,
        quantity: Quantity::parse(order.orig_sz.as_deref().unwrap_or(&order.sz))
            .map_err(|_| malformed("a size"))?,
        cumulative_filled: cumulative_filled(order.orig_sz.as_deref(), &order.sz)?,
        reduce_only: order.reduce_only.unwrap_or(false),
        // 挂在挂单列表里就是在场。
        status: OrderStatus::Open,
    })
}

/// 去重后的成交流水，按时间排出每张订单的累计成交量。
///
/// 这个累计量只在「窗口起点不晚于该订单挂出的时刻」时才是完整的。runtime 保证
/// 这一点：拉成交的起点取 `min(同步点 − 回看窗口, 最早在场订单的挂出时刻)`。
fn ordered_fills(unique: HashMap<u64, FillResponse>) -> Vec<ExchangeFill> {
    let mut fills: Vec<FillResponse> = unique.into_values().collect();
    fills.sort_by_key(|fill| (fill.time, fill.tid));

    let mut cumulative: HashMap<String, Quantity> = HashMap::new();
    let mut result = Vec::with_capacity(fills.len());
    for fill in fills {
        // 没有 `cloid` 的成交不是本系统下的单。
        let Some(client_order_id) = fill.cloid.as_deref().and_then(client_order_id_of) else {
            continue;
        };
        let (Ok(price), Ok(quantity)) = (Price::parse(&fill.px), Quantity::parse(&fill.sz)) else {
            continue;
        };
        // 会 `continue` 的检查都放在累计之前：被丢掉的成交如果先算进了累计量，
        // 后面几笔的累计量就会偏高，运行时拿它对账会把已入账的量当成没入账。
        let Some(filled_at) = DateTime::from_timestamp_millis(fill.time) else {
            continue;
        };
        let running = cumulative
            .entry(client_order_id.to_string())
            .or_insert(Quantity::ZERO);
        *running = running.saturating_add(quantity);
        result.push(ExchangeFill {
            trade_id: fill.tid.to_string(),
            client_order_id,
            price,
            quantity,
            cumulative_quantity: *running,
            filled_at,
        });
    }
    result
}

/// 下单回执。一个动作只发一张单，状态条数不等于 1 时无法归属，按结果未知处理。
fn order_receipt(response: &Value) -> Result<OrderReceipt, ExchangeError> {
    match response.get("status").and_then(Value::as_str) {
        Some("ok") => {}
        Some("err") => {
            return Err(ExchangeError::Rejected {
                reason: exchange_message(response),
            });
        }
        other => {
            return Err(ExchangeError::Unknown {
                reason: format!(
                    "the exchange returned an unexpected response status: {}",
                    other.unwrap_or("<missing>")
                ),
            });
        }
    }
    let statuses = response
        .pointer("/response/data/statuses")
        .and_then(Value::as_array)
        .filter(|statuses| statuses.len() == 1)
        .ok_or_else(|| ExchangeError::Unknown {
            reason: "the exchange did not return exactly one order status".to_string(),
        })?;
    let status = &statuses[0];
    if let Some(error) = status.get("error").and_then(Value::as_str) {
        return Err(ExchangeError::Rejected {
            reason: error.to_string(),
        });
    }
    if let Some(resting) = status.get("resting") {
        return Ok(OrderReceipt {
            exchange_order_id: oid_of(resting)?,
            cumulative_filled: Quantity::ZERO,
            status: OrderStatus::Open,
        });
    }
    if let Some(filled) = status.get("filled") {
        let total = filled.get("totalSz").and_then(Value::as_str).unwrap_or("0");
        let cumulative_filled = Quantity::parse(total).map_err(|_| malformed("a filled size"))?;
        return Ok(OrderReceipt {
            exchange_order_id: oid_of(filled)?,
            cumulative_filled,
            status: OrderStatus::Filled,
        });
    }
    Err(ExchangeError::Unknown {
        reason: "the exchange returned an order status this adapter does not know".to_string(),
    })
}

/// 撤单回执。交易所说「已经撤过或已成交」时，这张单的结局只能靠查状态定，
/// 所以要报「不认识这张单」而不是「拒绝」——后者会让调用方释放预留。
fn cancel_outcome(response: &Value, client_order_id: &ClientOrderId) -> Result<(), ExchangeError> {
    match response.get("status").and_then(Value::as_str) {
        Some("ok") => {}
        Some("err") => {
            return Err(ExchangeError::Rejected {
                reason: exchange_message(response),
            });
        }
        other => {
            return Err(ExchangeError::Unknown {
                reason: format!(
                    "the exchange returned an unexpected response status: {}",
                    other.unwrap_or("<missing>")
                ),
            });
        }
    }
    let statuses = response
        .pointer("/response/data/statuses")
        .and_then(Value::as_array)
        .filter(|statuses| statuses.len() == 1)
        .ok_or_else(|| ExchangeError::Unknown {
            reason: "the exchange did not return exactly one cancel status".to_string(),
        })?;
    match &statuses[0] {
        Value::String(status) if status == "success" => Ok(()),
        Value::Object(fields) => {
            let error = fields
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or_default();
            // 交易所的原话是
            // `Order was never placed, already canceled, or filled. asset=0`；
            // 它以前也说过 `Order has already been canceled or filled.`。
            // 所以按「撤掉 / 成交 / 压根没落地 / 不认识」这几个词判，而不是认整句——
            // 措辞一变就漏判，会把「已经不在场」说成「拒绝」。
            let error = error.to_ascii_lowercase();
            if error.contains("never placed")
                || error.contains("canceled")
                || error.contains("cancelled")
                || error.contains("filled")
                || error.contains("unknown")
            {
                return Err(ExchangeError::UnknownOrder {
                    client_order_id: client_order_id.clone(),
                });
            }
            Err(ExchangeError::Rejected {
                reason: exchange_message(response),
            })
        }
        _ => Err(ExchangeError::Unknown {
            reason: "the exchange returned a cancel status this adapter does not know".to_string(),
        }),
    }
}

fn oid_of(status: &Value) -> Result<String, ExchangeError> {
    status
        .get("oid")
        .and_then(Value::as_u64)
        .map(|oid| oid.to_string())
        .ok_or_else(|| malformed("an order id"))
}

/// `{"status":"err","response":"…"}` 里的那句话。
fn exchange_message(response: &Value) -> String {
    response
        .get("response")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "the exchange refused the action".to_string())
}

fn rejected(reason: String) -> ExchangeError {
    ExchangeError::Rejected { reason }
}

fn malformed(what: &str) -> ExchangeError {
    ExchangeError::Unavailable {
        reason: format!("the exchange returned a response with an unreadable {what}"),
    }
}
