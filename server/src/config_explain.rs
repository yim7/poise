use anyhow::{Result, ensure};
use poise_core::instrument::{Amount, InstrumentSpec, PositionBasis};
use poise_core::track::TrackDefinition;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TrackConfigExplanation {
    pub track_id: String,
    pub symbol: String,
    pub position_basis: PositionBasis,
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
    instrument_spec: &InstrumentSpec,
) -> Result<TrackConfigExplanation> {
    let native_quantity_per_unit = native_quantity_per_unit(track, instrument_spec)?;
    ensure!(
        native_quantity_per_unit + f64::EPSILON >= instrument_spec.min_position_qty().value(),
        "track `{}` symbol `{}` native_quantity_per_unit {} is below min_qty {}",
        track.track_id().as_str(),
        track.instrument().symbol,
        native_quantity_per_unit,
        instrument_spec.min_position_qty().value()
    );
    Ok(TrackConfigExplanation {
        track_id: track.track_id().as_str().to_string(),
        symbol: track.instrument().symbol.clone(),
        position_basis: instrument_spec.position_basis().clone(),
        native_quantity_per_unit,
        unit_notional: track.track_config().notional_per_unit,
        unit_notional_asset: instrument_spec.notional_asset().as_str().to_string(),
        quantity_step: instrument_spec.position_qty_step().value(),
        min_quantity: instrument_spec.min_position_qty().value(),
        min_notional: instrument_spec.min_notional().value(),
        effective_max_notional: track.effective_max_notional(),
        loss_limit_asset: instrument_spec.settlement_asset().as_str().to_string(),
        daily_loss_limit: track.loss_limits().daily_loss_limit,
        total_loss_limit: track.loss_limits().total_loss_limit,
    })
}

fn native_quantity_per_unit(
    track: &TrackDefinition,
    instrument_spec: &InstrumentSpec,
) -> Result<f64> {
    let band_center = track.track_config().band_center();
    ensure!(
        band_center.is_finite() && band_center > 0.0,
        "invalid band center for `{}`: got {}",
        track.instrument().symbol,
        band_center
    );
    instrument_spec
        .position_qty_for_notional(
            Amount::new(
                track.track_config().notional_per_unit,
                instrument_spec.notional_asset().clone(),
            )
            .map_err(anyhow::Error::msg)?,
            band_center,
        )
        .map(|quantity| quantity.value())
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use poise_core::instrument::{Amount, Asset, InstrumentSpec, PositionBasis};
    use poise_core::risk::LossLimits;
    use poise_core::strategy::{BandProtectionPolicy, ShapeFamily, TrackConfig};
    use poise_core::track::{Instrument, TrackDefinition, TrackId, Venue};

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
        let usd = Asset::try_new("USD").unwrap();
        let rules = InstrumentSpec::try_inverse(
            track.instrument().clone(),
            Asset::try_new("BTC").unwrap(),
            usd.clone(),
            Asset::try_new("BTC").unwrap(),
            Amount::new(100.0, usd.clone()).unwrap(),
            Default::default(),
            0.1,
            1.0,
            1.0,
            Amount::new(0.0, usd).unwrap(),
        )
        .unwrap();

        let explanation = explain_track_config(&track, &rules).unwrap();

        assert_eq!(explanation.track_id, "btc-core");
        assert_eq!(explanation.symbol, "BTC-USD-SWAP");
        assert!(matches!(
            explanation.position_basis,
            PositionBasis::Contracts { .. }
        ));
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
        let usdt = Asset::try_new("USDT").unwrap();
        let rules = InstrumentSpec::try_linear(
            track.instrument().clone(),
            Asset::try_new("ETH").unwrap(),
            usdt.clone(),
            usdt.clone(),
            Default::default(),
            0.1,
            0.001,
            0.01,
            Amount::new(5.0, usdt).unwrap(),
        )
        .unwrap();

        let explanation = explain_track_config(&track, &rules).unwrap();

        assert_eq!(explanation.native_quantity_per_unit, 0.05);
        assert_eq!(explanation.unit_notional, 200.0);
        assert_eq!(explanation.unit_notional_asset, "USDT");
        assert_eq!(explanation.effective_max_notional, 1_000.0);
        assert_eq!(explanation.loss_limit_asset, "USDT");
    }
}
