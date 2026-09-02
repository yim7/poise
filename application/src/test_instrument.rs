use poise_core::instrument::{Amount, Asset, InstrumentSpec};
use poise_core::track::{Instrument, Venue};

pub(crate) fn linear_spec(
    price_tick: f64,
    quantity_step: f64,
    min_quantity: f64,
    min_notional: f64,
) -> InstrumentSpec {
    let quote_asset = Asset::try_new("USDT").unwrap();
    InstrumentSpec::try_linear(
        Instrument::new(Venue::Binance, "BTCUSDT"),
        Asset::try_new("BTC").unwrap(),
        quote_asset.clone(),
        quote_asset.clone(),
        Default::default(),
        price_tick,
        quantity_step,
        min_quantity,
        Amount::new(min_notional, quote_asset).unwrap(),
    )
    .unwrap()
}

pub(crate) fn inverse_spec() -> InstrumentSpec {
    let usd = Asset::try_new("USD").unwrap();
    InstrumentSpec::try_inverse(
        Instrument::new(Venue::Okx, "BTC-USD-SWAP"),
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
    .unwrap()
}
