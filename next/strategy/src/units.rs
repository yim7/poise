//! 定点数值。
//!
//! 价格、数量、金额、仓位都用整数最小单位表示，不用二进制浮点做判断：
//! - 价格、数量、仓位、费率以 1e-8 为最小单位；
//! - 数量与价格相乘得到的金额以 1e-16 为最小单位（[`Notional`]），保留完整精度，
//!   使最低金额、扣费后盈利这类边界判断不受取整影响。

use std::fmt;
use std::ops::{Add, Sub};
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 定点小数的最小单位：1e-8。
pub const SCALE: i64 = 100_000_000;
const SCALE_RAW: i128 = SCALE as i128;
const MAX_DECIMALS: usize = 8;

/// 定点小数解析失败。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnitsError {
    #[error("`{value}` is not a decimal number")]
    NotANumber { value: String },
    #[error("`{value}` has more than {MAX_DECIMALS} decimal places")]
    TooManyDecimals { value: String },
    #[error("`{value}` is out of range")]
    OutOfRange { value: String },
}

/// 价格规整方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceRounding {
    Down,
    Up,
    Nearest,
}

fn parse_fixed(value: &str) -> Result<i64, UnitsError> {
    let trimmed = value.trim();
    let invalid = || UnitsError::NotANumber {
        value: trimmed.to_string(),
    };
    let out_of_range = || UnitsError::OutOfRange {
        value: trimmed.to_string(),
    };

    let (negative, digits) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let (integer, fraction) = match digits.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (digits, ""),
    };
    if integer.is_empty() && fraction.is_empty() {
        return Err(invalid());
    }
    if !integer.chars().all(|digit| digit.is_ascii_digit())
        || !fraction.chars().all(|digit| digit.is_ascii_digit())
    {
        return Err(invalid());
    }
    if fraction.len() > MAX_DECIMALS {
        return Err(UnitsError::TooManyDecimals {
            value: trimmed.to_string(),
        });
    }

    let integer: i64 = if integer.is_empty() {
        0
    } else {
        integer.parse().map_err(|_| out_of_range())?
    };
    let fraction_digits = fraction.len();
    let fraction: i64 = if fraction.is_empty() {
        0
    } else {
        fraction.parse().map_err(|_| out_of_range())?
    };
    let fraction_units = fraction * 10_i64.pow((MAX_DECIMALS - fraction_digits) as u32);
    let units = integer
        .checked_mul(SCALE)
        .and_then(|scaled| scaled.checked_add(fraction_units))
        .ok_or_else(out_of_range)?;
    Ok(if negative { -units } else { units })
}

fn format_fixed(units: i64) -> String {
    let sign = if units < 0 { "-" } else { "" };
    let magnitude = units.unsigned_abs();
    let integer = magnitude / SCALE as u64;
    let fraction = magnitude % SCALE as u64;
    if fraction == 0 {
        return format!("{sign}{integer}");
    }
    let mut fraction = format!("{fraction:0MAX_DECIMALS$}");
    while fraction.ends_with('0') {
        fraction.pop();
    }
    format!("{sign}{integer}.{fraction}")
}

/// 非负整数之间的向上取整除法。
///
/// 工具链还没有稳定 `div_ceil`，这里自己实现。
pub(crate) fn div_ceil(numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0, "denominator must be positive");
    debug_assert!(numerator >= 0, "numerator must be non-negative");
    (numerator + denominator - 1) / denominator
}

/// 非负整数之间的向下取整除法。
pub(crate) fn div_floor(numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0, "denominator must be positive");
    debug_assert!(numerator >= 0, "numerator must be non-negative");
    numerator / denominator
}

/// 非负整数之间的四舍五入除法（.5 向上）。
pub(crate) fn div_round(numerator: i128, denominator: i128) -> i128 {
    debug_assert!(denominator > 0, "denominator must be positive");
    debug_assert!(numerator >= 0, "numerator must be non-negative");
    (2 * numerator + denominator) / (2 * denominator)
}

/// 把 `total_steps` 个最小单位平均分成 `count` 份，除不尽的余数依次多给前几份。
///
/// 「前」指下标小的那一份，也就是靠近两侧之间空隙的那一端：多头侧的批次与格子按价格
/// 从高到低编号，空头侧从低到高，两侧的下标 0 都是最靠近空隙的那一份。
pub(crate) fn split_steps(total_steps: i64, index: usize, count: usize) -> i64 {
    debug_assert!(count > 0, "count must be positive");
    let base = total_steps / count as i64;
    let remainder = total_steps % count as i64;
    base + i64::from((index as i64) < remainder)
}

/// 把非负整数单位规整到 `step` 的整数倍，用 i128 算中间量。
///
/// 返回值可能超过 `i64`：向上取整时 `(商 + 1) × step` 在接近上限的输入上就会越界。
/// 调用方必须自己决定越界怎么办，不能让它变成 debug 构建里的 panic，也不能悄悄饱和成
/// 一个不是 `step` 整数倍的值。
fn round_units_to_step(units: i64, step: i64, rounding: PriceRounding) -> i128 {
    debug_assert!(step > 0, "step must be positive");
    debug_assert!(units >= 0, "units must be non-negative");
    let units = i128::from(units);
    let step = i128::from(step);
    let quotient = units / step;
    let remainder = units % step;
    match rounding {
        PriceRounding::Down => quotient * step,
        PriceRounding::Up => {
            if remainder == 0 {
                units
            } else {
                (quotient + 1) * step
            }
        }
        PriceRounding::Nearest => {
            if remainder * 2 >= step {
                (quotient + 1) * step
            } else {
                quotient * step
            }
        }
    }
}

macro_rules! fixed_point_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(i64);

        impl $name {
            pub const ZERO: Self = Self(0);

            /// 以 1e-8 为单位构造。
            pub const fn from_units(units: i64) -> Self {
                Self(units)
            }

            pub const fn units(self) -> i64 {
                self.0
            }

            /// 解析十进制字符串，最多 8 位小数。
            pub fn parse(value: &str) -> Result<Self, UnitsError> {
                Ok(Self(parse_fixed(value)?))
            }

            pub const fn is_zero(self) -> bool {
                self.0 == 0
            }

            pub const fn min(self, other: Self) -> Self {
                Self(if self.0 < other.0 { self.0 } else { other.0 })
            }

            pub const fn max(self, other: Self) -> Self {
                Self(if self.0 > other.0 { self.0 } else { other.0 })
            }

            /// `10^exponent`；不足一个最小单位时按一个最小单位算。
            pub fn power_of_ten(exponent: i32) -> Self {
                let decimals = exponent + MAX_DECIMALS as i32;
                if decimals <= 0 {
                    return Self(1);
                }
                if decimals > 18 {
                    return Self(i64::MAX);
                }
                Self(10_i64.pow(decimals as u32))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&format_fixed(self.0))
            }
        }

        impl FromStr for $name {
            type Err = UnitsError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct FixedPointVisitor;

                impl Visitor<'_> for FixedPointVisitor {
                    type Value = i64;

                    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                        formatter.write_str("a decimal number or its string form")
                    }

                    fn visit_str<E: de::Error>(self, value: &str) -> Result<i64, E> {
                        parse_fixed(value).map_err(de::Error::custom)
                    }

                    fn visit_i64<E: de::Error>(self, value: i64) -> Result<i64, E> {
                        value
                            .checked_mul(SCALE)
                            .ok_or_else(|| de::Error::custom("value out of range"))
                    }

                    fn visit_u64<E: de::Error>(self, value: u64) -> Result<i64, E> {
                        i64::try_from(value)
                            .ok()
                            .and_then(|value| value.checked_mul(SCALE))
                            .ok_or_else(|| de::Error::custom("value out of range"))
                    }

                    fn visit_f64<E: de::Error>(self, value: f64) -> Result<i64, E> {
                        parse_fixed(&format!("{value}")).map_err(de::Error::custom)
                    }
                }

                deserializer
                    .deserialize_any(FixedPointVisitor)
                    .map($name)
            }
        }
    };
}

fixed_point_type!(
    /// 价格，1e-8 报价货币。
    Price
);

fixed_point_type!(
    /// 数量，1e-8 标的。
    Quantity
);

fixed_point_type!(
    /// 仓位，1e-8 标的，带符号：多头为正。
    Position
);

fixed_point_type!(
    /// 比率，1e-8，例如 taker 费率 0.00045 记为 45000。
    Rate
);

impl Price {
    /// 按报价精度规整；规整结果超出可表示范围时返回 `None`。
    pub fn checked_round_to_tick(self, tick: Price, rounding: PriceRounding) -> Option<Price> {
        debug_assert!(tick.units() > 0, "price tick must be positive");
        i64::try_from(round_units_to_step(self.0, tick.0, rounding))
            .ok()
            .map(Price)
    }

    /// 小于等于这个价格的十的幂次，也就是十进制位数减一：`95000` → 4，`0.00123` → -3。
    ///
    /// 零和负数没有量级，按 0 处理；调用方应当先把它们挡掉。
    pub fn decade(self) -> i32 {
        let mut magnitude = self.0.unsigned_abs();
        let mut decade: i32 = 0;
        while magnitude >= 10 {
            magnitude /= 10;
            decade += 1;
        }
        decade - MAX_DECIMALS as i32
    }

    pub const fn saturating_sub(self, other: Price) -> Price {
        Price(self.0.saturating_sub(other.0))
    }

    pub const fn saturating_add(self, other: Price) -> Price {
        Price(self.0.saturating_add(other.0))
    }

    /// 加法，超出可表示范围时返回 `None`。
    ///
    /// 价格由配置和合约规则算出，不能悄悄饱和：饱和出来的价格不是报价精度的整数倍，
    /// 会变成一张无法合法报出的订单。
    pub const fn checked_add(self, other: Price) -> Option<Price> {
        match self.0.checked_add(other.0) {
            Some(units) => Some(Price(units)),
            None => None,
        }
    }

    /// 减法，超出可表示范围时返回 `None`。空头侧的平仓价是减出来的，用得上。
    ///
    /// 价格是有符号的：减出来是零或负数时照样返回，由调用方判断这样的价格能不能挂出去。
    pub const fn checked_sub(self, other: Price) -> Option<Price> {
        match self.0.checked_sub(other.0) {
            Some(units) => Some(Price(units)),
            None => None,
        }
    }
}

impl Quantity {
    /// 按数量精度向下规整。
    pub fn floor_to_step(self, step: Quantity) -> Quantity {
        debug_assert!(step.units() > 0, "quantity step must be positive");
        // 向下取整的结果不会超过原值，不可能越界。
        Quantity(round_units_to_step(self.0, step.0, PriceRounding::Down) as i64)
    }

    /// 按数量精度向上规整；规整结果超出可表示范围时返回 `None`。
    pub fn checked_ceil_to_step(self, step: Quantity) -> Option<Quantity> {
        debug_assert!(step.units() > 0, "quantity step must be positive");
        i64::try_from(round_units_to_step(self.0, step.0, PriceRounding::Up))
            .ok()
            .map(Quantity)
    }

    /// 乘以一个比率，向上取整到最小单位。
    pub fn mul_rate_ceil(self, rate: Rate) -> Quantity {
        debug_assert!(self.0 >= 0, "quantity must be non-negative");
        debug_assert!(rate.0 >= 0, "rate must be non-negative");
        let scaled = self.0 as i128 * rate.0 as i128;
        Quantity(div_ceil(scaled, SCALE_RAW) as i64)
    }

    pub const fn saturating_sub(self, other: Quantity) -> Quantity {
        Quantity(self.0.saturating_sub(other.0))
    }

    pub const fn saturating_add(self, other: Quantity) -> Quantity {
        Quantity(self.0.saturating_add(other.0))
    }
}

impl Position {
    pub const fn from_quantity(quantity: Quantity) -> Self {
        Self(quantity.units())
    }

    /// 仓位非负时返回数量。
    pub const fn to_quantity(self) -> Option<Quantity> {
        if self.0 >= 0 {
            Some(Quantity(self.0))
        } else {
            None
        }
    }
}

impl Add for Quantity {
    type Output = Quantity;

    fn add(self, other: Quantity) -> Quantity {
        Quantity(self.0 + other.0)
    }
}

/// 金额，1e-16 报价货币：数量与价格相乘的结果，保留完整精度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Notional(i128);

impl Notional {
    pub const ZERO: Self = Self(0);

    /// 数量 × 价格。
    pub fn of(quantity: Quantity, price: Price) -> Self {
        Self(quantity.units() as i128 * price.units() as i128)
    }

    /// 以 1e-8 报价货币为单位构造，例如最低订单金额 10 美元。
    pub fn from_scaled(units: i64) -> Self {
        Self(units as i128 * SCALE_RAW)
    }

    pub fn parse(value: &str) -> Result<Self, UnitsError> {
        Ok(Self::from_scaled(parse_fixed(value)?))
    }

    pub const fn raw(self) -> i128 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub fn checked_mul(self, factor: i64) -> Option<Self> {
        self.0.checked_mul(factor as i128).map(Self)
    }

    /// 按 1e-8 报价货币取值，向下取整。
    pub const fn scaled_floor(self) -> i64 {
        (self.0 / SCALE_RAW) as i64
    }

    /// 按费率取一部分（手续费），向下取整。
    ///
    /// 向下取整让算出来的费用偏小：它用在「扣费后还剩多少」的展示上，真正的拒绝
    /// 判断由 [`crate::grid::spread_is_profitable`] 用整数比较完成，不经过这里。
    pub fn mul_rate(self, rate: Rate) -> Self {
        debug_assert!(rate.0 >= 0, "rate must be non-negative");
        Self(self.0 * rate.0 as i128 / SCALE_RAW)
    }
}

impl Add for Notional {
    type Output = Notional;

    fn add(self, other: Notional) -> Notional {
        Notional(self.0 + other.0)
    }
}

impl Sub for Notional {
    type Output = Notional;

    /// 收益这类结果可以是负的，所以这里不做饱和。
    fn sub(self, other: Notional) -> Notional {
        Notional(self.0 - other.0)
    }
}

impl fmt::Display for Notional {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&format_fixed(self.scaled_floor()))
    }
}

impl Serialize for Notional {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Notional {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NotionalVisitor;

        impl Visitor<'_> for NotionalVisitor {
            type Value = Notional;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an amount in quote currency or its string form")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Notional, E> {
                Notional::parse(value).map_err(de::Error::custom)
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Notional, E> {
                value
                    .checked_mul(SCALE)
                    .map(Notional::from_scaled)
                    .ok_or_else(|| de::Error::custom("value out of range"))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Notional, E> {
                i64::try_from(value)
                    .ok()
                    .and_then(|value| value.checked_mul(SCALE))
                    .map(Notional::from_scaled)
                    .ok_or_else(|| de::Error::custom("value out of range"))
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Notional, E> {
                Notional::parse(&format!("{value}")).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_any(NotionalVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Notional, Position, Price, PriceRounding, Quantity, Rate, UnitsError, format_fixed,
        parse_fixed,
    };

    #[test]
    fn parses_and_formats_decimals_exactly() {
        assert_eq!(Price::parse("75000").unwrap().units(), 75_000 * 100_000_000);
        assert_eq!(Quantity::parse("0.015").unwrap().units(), 1_500_000);
        assert_eq!(Quantity::parse(".5").unwrap().units(), 50_000_000);
        assert_eq!(Price::parse("95.").unwrap().units(), 95 * 100_000_000);
        assert_eq!(Quantity::parse("-0.25").unwrap().units(), -25_000_000);
        assert_eq!(Quantity::parse("0.015").unwrap().to_string(), "0.015");
        assert_eq!(Price::parse("75000").unwrap().to_string(), "75000");
        assert_eq!(Quantity::parse("1.50000000").unwrap().to_string(), "1.5");
        assert_eq!(parse_fixed("0.00000001").unwrap(), 1);
        assert_eq!(format_fixed(1), "0.00000001");
    }

    #[test]
    fn rejects_malformed_or_too_precise_decimals() {
        assert!(matches!(
            Quantity::parse("0.000000001"),
            Err(UnitsError::TooManyDecimals { .. })
        ));
        assert!(matches!(
            Price::parse("9.5.5"),
            Err(UnitsError::NotANumber { .. })
        ));
        assert!(matches!(
            Price::parse(""),
            Err(UnitsError::NotANumber { .. })
        ));
        assert!(matches!(
            Price::parse("1e5"),
            Err(UnitsError::NotANumber { .. })
        ));
    }

    #[test]
    fn rounds_prices_to_tick_in_each_direction() {
        let tick = Price::parse("1").unwrap();
        let price = Price::parse("94.6").unwrap();

        assert_eq!(
            price.checked_round_to_tick(tick, PriceRounding::Down),
            Some(Price::parse("94").unwrap())
        );
        assert_eq!(
            price.checked_round_to_tick(tick, PriceRounding::Up),
            Some(Price::parse("95").unwrap())
        );
        assert_eq!(
            price.checked_round_to_tick(tick, PriceRounding::Nearest),
            Some(Price::parse("95").unwrap())
        );
        assert_eq!(
            Price::parse("94.5")
                .unwrap()
                .checked_round_to_tick(tick, PriceRounding::Nearest),
            Some(Price::parse("95").unwrap())
        );
        assert_eq!(
            Price::parse("94")
                .unwrap()
                .checked_round_to_tick(tick, PriceRounding::Up),
            Some(Price::parse("94").unwrap())
        );
    }

    /// 向上规整到 `tick` 的整数倍可能超过 `i64`：这个输入在 debug 构建里
    /// 曾经直接 panic（`(商 + 1) × step` 越界）。
    #[test]
    fn reports_prices_whose_rounded_value_is_not_representable() {
        let tick = Price::parse("1").unwrap();
        let price = Price::parse("92233720368.54").unwrap();

        assert_eq!(
            price.checked_round_to_tick(tick, PriceRounding::Down),
            Some(Price::parse("92233720368").unwrap())
        );
        assert_eq!(
            price.checked_round_to_tick(tick, PriceRounding::Nearest),
            None
        );
        assert_eq!(price.checked_round_to_tick(tick, PriceRounding::Up), None);
    }

    /// 数量向上规整同样可能越界。
    #[test]
    fn reports_quantities_whose_rounded_value_is_not_representable() {
        let step = Quantity::parse("0.00000003").unwrap();
        let quantity = Quantity::parse("92233720368.54775807").unwrap();

        assert_eq!(
            quantity.checked_ceil_to_step(step),
            None,
            "rounding up past i64 must be reported, not wrapped"
        );
    }

    #[test]
    fn floors_quantities_to_step() {
        let step = Quantity::parse("0.001").unwrap();

        assert_eq!(
            Quantity::parse("0.0159").unwrap().floor_to_step(step),
            Quantity::parse("0.015").unwrap()
        );
        assert_eq!(
            Quantity::parse("0.015").unwrap().floor_to_step(step),
            Quantity::parse("0.015").unwrap()
        );
    }

    #[test]
    fn multiplies_quantity_and_price_without_losing_precision() {
        let notional = Notional::of(
            Quantity::parse("0.1").unwrap(),
            Price::parse("99.99").unwrap(),
        );

        assert_eq!(notional, Notional::parse("9.999").unwrap());
        assert!(notional < Notional::parse("10").unwrap());
        assert_eq!(notional.scaled_floor(), 999_900_000);
        assert_eq!(notional.to_string(), "9.999");
        assert_eq!(
            notional,
            Notional::of(
                Quantity::parse("1").unwrap(),
                Price::parse("9.999").unwrap()
            )
        );
    }

    #[test]
    fn positions_are_signed_quantities() {
        assert_eq!(
            Position::from_quantity(Quantity::parse("0.37").unwrap()),
            Position::parse("0.37").unwrap()
        );
        assert_eq!(
            Position::parse("0.37").unwrap().to_quantity(),
            Some(Quantity::parse("0.37").unwrap())
        );
        assert_eq!(Position::parse("-0.25").unwrap().to_quantity(), None);
    }

    #[test]
    fn rates_round_trip_through_json() {
        let rate = Rate::parse("0.00045").unwrap();

        let json = serde_json::to_string(&rate).unwrap();

        assert_eq!(json, "\"0.00045\"");
        assert_eq!(serde_json::from_str::<Rate>(&json).unwrap(), rate);
        assert_eq!(
            serde_json::from_str::<Rate>("0.00045").unwrap(),
            Rate::parse("0.00045").unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Quantity>("0.015").unwrap(),
            Quantity::parse("0.015").unwrap()
        );
    }
}
