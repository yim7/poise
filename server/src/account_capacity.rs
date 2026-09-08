use anyhow::{Context, Result, ensure};
use poise_application::exchange::AccountSummarySnapshot;
use poise_core::instrument::{Amount, Asset, InstrumentSpec, PositionBasis};

pub(crate) struct AccountCapacityEstimate {
    pub(crate) available: Amount,
    pub(crate) max_increase_notional: f64,
}

pub(crate) fn estimate_account_capacity(
    account: &AccountSummarySnapshot,
    instrument_spec: &InstrumentSpec,
    mark_price: Option<f64>,
    leverage: u32,
) -> Result<AccountCapacityEstimate> {
    if let Some(mark_price) = mark_price {
        ensure!(
            mark_price.is_finite() && mark_price > 0.0,
            "invalid mark price for account capacity on `{}`: got {}",
            instrument_spec.instrument().symbol,
            mark_price
        );
    }

    let required_asset = match instrument_spec.position_basis() {
        PositionBasis::BaseAsset => instrument_spec.notional_asset(),
        PositionBasis::Contracts { .. } => instrument_spec.settlement_asset(),
    };
    let available = non_negative_available_balance(account, required_asset)?;
    let max_increase_notional = match instrument_spec.position_basis() {
        PositionBasis::BaseAsset => available.value() * leverage as f64,
        PositionBasis::Contracts { .. } => {
            let mark_price = mark_price.with_context(|| {
                format!(
                    "missing mark price for inverse account capacity on `{}`",
                    instrument_spec.instrument().symbol
                )
            })?;
            available.value() * mark_price * leverage as f64
        }
    };
    ensure!(
        max_increase_notional.is_finite(),
        "account capacity for `{}` must be finite, got {}",
        instrument_spec.instrument().symbol,
        max_increase_notional
    );

    Ok(AccountCapacityEstimate {
        available,
        max_increase_notional,
    })
}

fn non_negative_available_balance(
    account: &AccountSummarySnapshot,
    required_asset: &Asset,
) -> Result<Amount> {
    if let Some(value) = account.available_for_asset(required_asset.as_str()) {
        return Amount::new(value.max(0.0), required_asset.clone()).map_err(anyhow::Error::msg);
    }

    let available = account.available().with_context(|| {
        format!(
            "missing available balance for required asset `{}`",
            required_asset.as_str()
        )
    })?;
    ensure!(
        available.asset() == required_asset,
        "account-wide available asset `{}` does not match required asset `{}`",
        available.asset().as_str(),
        required_asset.as_str()
    );
    Amount::new(available.value().max(0.0), required_asset.clone()).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_core::instrument::{Amount, Asset, InstrumentSpec};
    use poise_core::track::{Instrument, Venue};

    use super::estimate_account_capacity;

    #[test]
    fn per_asset_negative_available_produces_zero_capacity() {
        let account = crate::test_support::test_account_summary_snapshot(
            "USD",
            1_000.0,
            Some(500.0),
            &[("USDT", -25.0)],
            None,
            Utc::now(),
        );

        let capacity = estimate_account_capacity(&account, &linear_spec(), Some(100.0), 5).unwrap();

        assert_eq!(capacity.available.value(), 0.0);
        assert_eq!(capacity.available.asset().as_str(), "USDT");
        assert_eq!(capacity.max_increase_notional, 0.0);
    }

    #[test]
    fn account_wide_negative_available_produces_zero_capacity() {
        let account = crate::test_support::test_account_summary_snapshot(
            "USDT",
            1_000.0,
            Some(-25.0),
            &[],
            None,
            Utc::now(),
        );

        let capacity = estimate_account_capacity(&account, &linear_spec(), None, 5).unwrap();

        assert_eq!(capacity.available.value(), 0.0);
        assert_eq!(capacity.available.asset().as_str(), "USDT");
        assert_eq!(capacity.max_increase_notional, 0.0);
    }

    fn linear_spec() -> InstrumentSpec {
        let usdt = Asset::try_new("USDT").unwrap();
        InstrumentSpec::try_linear(
            Instrument::new(Venue::Binance, "BTCUSDT"),
            Asset::try_new("BTC").unwrap(),
            usdt.clone(),
            usdt.clone(),
            Default::default(),
            0.1,
            0.001,
            0.001,
            Amount::new(5.0, usdt).unwrap(),
        )
        .unwrap()
    }
}
