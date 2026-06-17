use anyhow::{Context, Result, ensure};
use poise_core::track::TrackDefinition;
use poise_core::types::{ExchangeRules, QuantityKind};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TrackConfigExplanation {
    pub track_id: String,
    pub symbol: String,
    pub quantity_kind: QuantityKind,
    pub native_quantity_per_unit: f64,
    pub unit_notional: f64,
    pub unit_notional_asset: String,
    pub quantity_step: f64,
    pub min_quantity: f64,
    pub min_notional: f64,
    pub effective_max_notional: f64,
    pub loss_limit_asset: String,
    pub daily_loss_limit: f64,
    pub total_loss_limit: f64,
}

pub(crate) fn explain_track_config(
    track: &TrackDefinition,
    exchange_rules: &ExchangeRules,
) -> Result<TrackConfigExplanation> {
    ensure!(
        !exchange_rules.settlement_asset.trim().is_empty(),
        "missing settlement asset for `{}`",
        track.instrument().symbol
    );
    ensure!(
        exchange_rules.quantity_step.is_finite() && exchange_rules.quantity_step > 0.0,
        "invalid quantity step for `{}`: quantity_step must be positive, got {}",
        track.instrument().symbol,
        exchange_rules.quantity_step
    );
    ensure!(
        exchange_rules.min_qty.is_finite() && exchange_rules.min_qty >= 0.0,
        "invalid minimum quantity for `{}`: min_qty must be non-negative, got {}",
        track.instrument().symbol,
        exchange_rules.min_qty
    );
    ensure!(
        exchange_rules.min_notional.is_finite() && exchange_rules.min_notional >= 0.0,
        "invalid minimum notional for `{}`: min_notional must be non-negative, got {}",
        track.instrument().symbol,
        exchange_rules.min_notional
    );

    let native_quantity_per_unit = native_quantity_per_unit(track, exchange_rules)?;
    ensure!(
        native_quantity_per_unit + f64::EPSILON >= exchange_rules.min_qty,
        "track `{}` symbol `{}` native_quantity_per_unit {} is below min_qty {}",
        track.track_id().as_str(),
        track.instrument().symbol,
        native_quantity_per_unit,
        exchange_rules.min_qty
    );
    Ok(TrackConfigExplanation {
        track_id: track.track_id().as_str().to_string(),
        symbol: track.instrument().symbol.clone(),
        quantity_kind: exchange_rules.quantity_kind,
        native_quantity_per_unit,
        unit_notional: track.track_config().notional_per_unit,
        unit_notional_asset: unit_notional_asset(track, exchange_rules),
        quantity_step: exchange_rules.quantity_step,
        min_quantity: exchange_rules.min_qty,
        min_notional: exchange_rules.min_notional,
        effective_max_notional: track.effective_max_notional(),
        loss_limit_asset: exchange_rules.settlement_asset.clone(),
        daily_loss_limit: track.loss_limits().daily_loss_limit,
        total_loss_limit: track.loss_limits().total_loss_limit,
    })
}

fn native_quantity_per_unit(
    track: &TrackDefinition,
    exchange_rules: &ExchangeRules,
) -> Result<f64> {
    match exchange_rules.quantity_kind {
        QuantityKind::BaseAsset => {
            let band_center = track.track_config().band_center();
            ensure!(
                band_center.is_finite() && band_center > 0.0,
                "invalid band center for `{}`: got {}",
                track.instrument().symbol,
                band_center
            );
            Ok(exchange_rules
                .native_qty_per_exposure_unit(track.track_config().notional_per_unit, band_center))
        }
        QuantityKind::InverseContract => {
            let contract_notional = exchange_rules.contract_notional.with_context(|| {
                format!(
                    "missing ctVal/contract_notional for inverse config explanation on `{}`",
                    track.instrument().symbol
                )
            })?;
            ensure!(
                contract_notional.is_finite() && contract_notional > 0.0,
                "invalid contract_notional for `{}`: got {}",
                track.instrument().symbol,
                contract_notional
            );
            Ok(track.track_config().notional_per_unit / contract_notional)
        }
    }
}

fn unit_notional_asset(track: &TrackDefinition, exchange_rules: &ExchangeRules) -> String {
    match exchange_rules.quantity_kind {
        QuantityKind::BaseAsset => track.instrument().quote_asset(),
        QuantityKind::InverseContract => "USD".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};
    use poise_core::types::{ExchangeRules, QuantityKind};

    use super::explain_track_config;

    #[test]
    fn explains_inverse_track_units_and_risk_assets() {
        let track = TrackDefinition::try_new(
            TrackId::new("btc-core"),
            Instrument::new(Venue::Okx, "BTC-USD-SWAP"),
            TrackConfig {
                lower_price: 60_000.0,
                upper_price: 70_000.0,
                long_exposure_units: 4.0,
                short_exposure_units: 6.0,
                notional_per_unit: 300.0,
                min_rebalance_units: 0.25,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: BandProtectionPolicy::Freeze,
                risk_acquisition: Default::default(),
            },
            Some(1_500.0),
            LossLimits {
                daily_loss_limit: 0.01,
                total_loss_limit: 0.03,
            },
            None,
        )
        .unwrap();
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: QuantityKind::InverseContract,
            contract_notional: Some(100.0),
            settlement_asset: "BTC".to_string(),
            quantity_step: 1.0,
            min_qty: 1.0,
            min_notional: 0.0,
            maker_fee_rate: 0.0002,
            taker_fee_rate: 0.0005,
        };

        let explanation = explain_track_config(&track, &rules).unwrap();

        assert_eq!(explanation.track_id, "btc-core");
        assert_eq!(explanation.symbol, "BTC-USD-SWAP");
        assert_eq!(explanation.quantity_kind, QuantityKind::InverseContract);
        assert_eq!(explanation.native_quantity_per_unit, 3.0);
        assert_eq!(explanation.unit_notional, 300.0);
        assert_eq!(explanation.unit_notional_asset, "USD");
        assert_eq!(explanation.quantity_step, 1.0);
        assert_eq!(explanation.min_quantity, 1.0);
        assert_eq!(explanation.min_notional, 0.0);
        assert_eq!(explanation.effective_max_notional, 1_500.0);
        assert_eq!(explanation.loss_limit_asset, "BTC");
        assert_eq!(explanation.daily_loss_limit, 0.01);
        assert_eq!(explanation.total_loss_limit, 0.03);
    }

    #[test]
    fn explains_base_asset_track_units_from_band_center() {
        let track = TrackDefinition::try_new(
            TrackId::new("eth-core"),
            Instrument::new(Venue::Binance, "ETHUSDT"),
            TrackConfig {
                lower_price: 3_000.0,
                upper_price: 5_000.0,
                long_exposure_units: 5.0,
                short_exposure_units: 5.0,
                notional_per_unit: 200.0,
                min_rebalance_units: 0.25,
                shape_family: ShapeFamily::Linear,
                out_of_band_policy: BandProtectionPolicy::Freeze,
                risk_acquisition: Default::default(),
            },
            None,
            LossLimits {
                daily_loss_limit: 100.0,
                total_loss_limit: 300.0,
            },
            None,
        )
        .unwrap();
        let rules = ExchangeRules {
            price_tick: 0.1,
            price_precision: Default::default(),
            quantity_kind: QuantityKind::BaseAsset,
            contract_notional: None,
            settlement_asset: "USDT".to_string(),
            quantity_step: 0.001,
            min_qty: 0.01,
            min_notional: 5.0,
            maker_fee_rate: 0.0002,
            taker_fee_rate: 0.0005,
        };

        let explanation = explain_track_config(&track, &rules).unwrap();

        assert_eq!(explanation.native_quantity_per_unit, 0.05);
        assert_eq!(explanation.unit_notional, 200.0);
        assert_eq!(explanation.unit_notional_asset, "USDT");
        assert_eq!(explanation.effective_max_notional, 1_000.0);
        assert_eq!(explanation.loss_limit_asset, "USDT");
    }
}
