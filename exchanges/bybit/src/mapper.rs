use anyhow::{Context, Result, anyhow};
use chrono::{TimeZone, Utc};

use poise_engine::ports::{AccountCapacitySnapshot, AccountSummarySnapshot, ExchangeInfo};
use poise_engine::track::{Instrument, Venue};

use crate::rest::models::{
    InstrumentInfoResult, ServerTimeResult, UnifiedWalletBalance, WalletBalanceResult,
};

pub(crate) fn build_account_capacity_snapshot(
    wallet_balance: &WalletBalanceResult,
) -> Result<AccountCapacitySnapshot> {
    let balance = first_wallet_balance(wallet_balance)?;
    let available = required_decimal(
        "totalAvailableBalance",
        balance.total_available_balance.as_deref(),
    )?;
    Ok(AccountCapacitySnapshot {
        max_increase_notional: available,
    })
}

impl TryFrom<InstrumentInfoResult> for ExchangeInfo {
    type Error = anyhow::Error;

    fn try_from(value: InstrumentInfoResult) -> Result<Self, Self::Error> {
        let info = value
            .list
            .into_iter()
            .next()
            .context("missing linear instrument info")?;
        Ok(Self {
            instrument: Instrument::new(Venue::Bybit, info.symbol),
            rules: poise_core::types::ExchangeRules {
                price_tick: required_decimal(
                    "priceFilter.tickSize",
                    info.price_filter.tick_size.as_deref(),
                )?,
                quantity_step: required_decimal(
                    "lotSizeFilter.qtyStep",
                    info.lot_size_filter.qty_step.as_deref(),
                )?,
                min_qty: required_decimal(
                    "lotSizeFilter.minOrderQty",
                    info.lot_size_filter.min_order_qty.as_deref(),
                )?,
                min_notional: required_decimal(
                    "lotSizeFilter.minNotionalValue",
                    info.lot_size_filter.min_notional_value.as_deref(),
                )?,
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.00055,
            },
        })
    }
}

impl WalletBalanceResult {
    pub(crate) fn into_account_summary_snapshot(self) -> Result<AccountSummarySnapshot> {
        let balance = first_wallet_balance(&self)?;
        Ok(AccountSummarySnapshot {
            equity: required_decimal("totalEquity", balance.total_equity.as_deref())?,
            available: required_decimal(
                "totalAvailableBalance",
                balance.total_available_balance.as_deref(),
            )?,
            unrealized_pnl: required_decimal("totalPerpUPL", balance.total_perp_upl.as_deref())?,
            observed_at: Utc::now(),
        })
    }
}

impl TryFrom<ServerTimeResult> for chrono::DateTime<Utc> {
    type Error = anyhow::Error;

    fn try_from(value: ServerTimeResult) -> Result<Self, Self::Error> {
        Utc.timestamp_opt(value.time_second, 0)
            .single()
            .ok_or_else(|| anyhow!("invalid Bybit server time: {}", value.time_second))
    }
}

fn first_wallet_balance(wallet_balance: &WalletBalanceResult) -> Result<&UnifiedWalletBalance> {
    let balance = wallet_balance
        .list
        .first()
        .context("missing unified wallet balance entry")?;
    if balance.account_type.as_deref() != Some("UNIFIED") {
        return Err(anyhow!(
            "expected wallet balance accountType=UNIFIED, got {}",
            balance.account_type.as_deref().unwrap_or("missing")
        ));
    }
    Ok(balance)
}

fn required_decimal(field: &str, value: Option<&str>) -> Result<f64> {
    let value = value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("missing required {field}"))?;
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rest::models::{
        InstrumentInfoResult, LinearInstrumentInfo, LotSizeFilter, PriceFilter, ServerTimeResult,
        UnifiedWalletBalance, WalletBalanceResult,
    };
    use poise_core::types::ExchangeRules;

    #[test]
    fn converts_linear_instrument_info_into_exchange_info() {
        let info = LinearInstrumentInfo {
            symbol: "BTCUSDT".to_string(),
            price_filter: PriceFilter {
                tick_size: Some("0.10".to_string()),
            },
            lot_size_filter: LotSizeFilter {
                qty_step: Some("0.001".to_string()),
                min_order_qty: Some("0.001".to_string()),
                min_notional_value: Some("5".to_string()),
            },
        };

        let exchange_info =
            ExchangeInfo::try_from(InstrumentInfoResult { list: vec![info] }).unwrap();

        assert_eq!(
            exchange_info.instrument,
            Instrument::new(Venue::Bybit, "BTCUSDT")
        );
        assert_eq!(
            exchange_info.rules,
            ExchangeRules {
                price_tick: 0.1,
                quantity_step: 0.001,
                min_qty: 0.001,
                min_notional: 5.0,
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.00055,
            }
        );
    }

    #[test]
    fn converts_unified_wallet_balance_into_account_summary_snapshot() {
        let balances = WalletBalanceResult {
            list: vec![UnifiedWalletBalance {
                account_type: Some("UNIFIED".to_string()),
                total_equity: Some("125.5".to_string()),
                total_available_balance: Some("100.25".to_string()),
                total_perp_upl: Some("-2.75".to_string()),
            }],
        };

        let snapshot = balances.into_account_summary_snapshot().unwrap();

        assert_eq!(snapshot.equity, 125.5);
        assert_eq!(snapshot.available, 100.25);
        assert_eq!(snapshot.unrealized_pnl, -2.75);
    }

    #[test]
    fn missing_unified_wallet_balance_fields_fail_stably() {
        let balances = WalletBalanceResult {
            list: vec![UnifiedWalletBalance {
                account_type: Some("UNIFIED".to_string()),
                total_equity: None,
                total_available_balance: Some("100.25".to_string()),
                total_perp_upl: Some("-2.75".to_string()),
            }],
        };

        let error = balances
            .into_account_summary_snapshot()
            .unwrap_err()
            .to_string();

        assert!(error.contains("totalEquity"));
    }

    #[test]
    fn non_unified_wallet_balance_entries_fail_stably() {
        let balances = WalletBalanceResult {
            list: vec![UnifiedWalletBalance {
                account_type: Some("CONTRACT".to_string()),
                total_equity: Some("125.5".to_string()),
                total_available_balance: Some("100.25".to_string()),
                total_perp_upl: Some("-2.75".to_string()),
            }],
        };

        let error = balances
            .into_account_summary_snapshot()
            .unwrap_err()
            .to_string();

        assert!(error.contains("accountType=UNIFIED"));
    }

    #[test]
    fn builds_account_capacity_snapshot_from_available_balance_only() {
        let balances = WalletBalanceResult {
            list: vec![UnifiedWalletBalance {
                account_type: Some("UNIFIED".to_string()),
                total_equity: Some("125.5".to_string()),
                total_available_balance: Some("100.25".to_string()),
                total_perp_upl: Some("-2.75".to_string()),
            }],
        };

        let snapshot = build_account_capacity_snapshot(&balances).unwrap();

        assert_eq!(snapshot.max_increase_notional, 100.25);
    }

    #[test]
    fn parses_bybit_server_time() {
        let response = ServerTimeResult {
            time_second: 1_700_000_000,
        };

        let time = chrono::DateTime::<Utc>::try_from(response).unwrap();

        assert_eq!(time.timestamp(), 1_700_000_000);
    }
}
