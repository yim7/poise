//! 资金预检（第 10 节）：一张会增加敞口的订单要预留多少保证金。
//!
//! 纯计算。可用资金、杠杆、费率由调用方从交易所读来，这里只做保守估算：所需保证金
//! 按「数量 × 限价 ÷ 杠杆 + 按 taker 费率估算的手续费」，各项都向上取整。

use crate::instrument::Fees;
use crate::units::{Notional, Price, Quantity};

/// 一张订单要预留的保证金：数量 × 限价 ÷ 杠杆 + 按 taker 费率估算的手续费。
///
/// 手续费按 taker 算：限价单通常是 maker，但会立即成交的单按 taker 收费，预检不区分，
/// 宁可多留。杠杆为 0 的配置不存在，按 1 倍算。
pub fn margin_required(
    quantity: Quantity,
    limit_price: Price,
    leverage: u32,
    fees: &Fees,
) -> Notional {
    let notional = Notional::of(quantity, limit_price);
    notional.div_ceil(leverage.max(1)) + notional.mul_rate_ceil(fees.taker_rate)
}

/// 预留这一张之后还够不够：所需保证金不超过可用资金减去已经预留的部分。
pub fn fits(available: Notional, reserved: Notional, required: Notional) -> bool {
    reserved + required <= available
}

#[cfg(test)]
mod tests {
    use super::{fits, margin_required};
    use crate::fixtures::btc_fees;
    use crate::units::{Notional, Price, Quantity};

    fn notional(value: &str) -> Notional {
        Notional::parse(value).unwrap()
    }

    /// .05 × 90000 = 4500：20 倍杠杆占 225，taker 0.045% 手续费 2.025。
    #[test]
    fn the_margin_is_notional_over_leverage_plus_the_taker_fee() {
        let required = margin_required(
            Quantity::parse("0.05").unwrap(),
            Price::parse("90000").unwrap(),
            20,
            &btc_fees(),
        );

        assert_eq!(required, notional("227.025"));
    }

    #[test]
    fn an_order_fits_while_the_reserved_total_stays_within_the_available_funds() {
        assert!(fits(notional("1000"), notional("700"), notional("300")));
        assert!(!fits(
            notional("1000"),
            notional("700"),
            notional("300.00000001")
        ));
    }
}
