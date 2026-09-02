use serde::{Deserialize, Serialize};

use crate::track::Instrument;
use crate::types::{PricePrecision, PriceRounding};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Asset(String);

impl Asset {
    pub fn try_new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        let value = value.trim();
        if value.is_empty() {
            return Err("asset must not be empty".to_string());
        }
        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Amount {
    value: f64,
    asset: Asset,
}

impl Amount {
    pub fn new(value: f64, asset: Asset) -> Result<Self, String> {
        if !value.is_finite() {
            return Err(format!("amount must be finite, got {value}"));
        }
        Ok(Self { value, asset })
    }

    pub fn value(&self) -> f64 {
        self.value
    }

    pub fn asset(&self) -> &Asset {
        &self.asset
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PositionQty(f64);

impl PositionQty {
    pub fn new(value: f64) -> Result<Self, String> {
        if !value.is_finite() {
            return Err(format!("position quantity must be finite, got {value}"));
        }
        Ok(Self(value))
    }

    pub fn value(self) -> f64 {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PositionBasis {
    BaseAsset,
    Contracts { contract_value: Amount },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstrumentSpec {
    instrument: Instrument,
    base_asset: Asset,
    quote_asset: Asset,
    notional_asset: Asset,
    settlement_asset: Asset,
    position_basis: PositionBasis,
    price_precision: PricePrecision,
    price_tick: f64,
    position_qty_step: PositionQty,
    min_position_qty: PositionQty,
    min_notional: Amount,
}

impl InstrumentSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn try_linear(
        instrument: Instrument,
        base_asset: Asset,
        quote_asset: Asset,
        settlement_asset: Asset,
        price_precision: PricePrecision,
        price_tick: f64,
        position_qty_step: f64,
        min_position_qty: f64,
        min_notional: Amount,
    ) -> Result<Self, String> {
        let notional_asset = min_notional.asset.clone();
        Self::try_new(
            instrument,
            base_asset,
            quote_asset,
            notional_asset,
            settlement_asset,
            PositionBasis::BaseAsset,
            price_precision,
            price_tick,
            position_qty_step,
            min_position_qty,
            min_notional,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn try_inverse(
        instrument: Instrument,
        base_asset: Asset,
        quote_asset: Asset,
        settlement_asset: Asset,
        contract_value: Amount,
        price_precision: PricePrecision,
        price_tick: f64,
        position_qty_step: f64,
        min_position_qty: f64,
        min_notional: Amount,
    ) -> Result<Self, String> {
        let notional_asset = contract_value.asset.clone();
        if min_notional.asset != notional_asset {
            return Err(format!(
                "minimum notional asset `{}` does not match contract value asset `{}`",
                min_notional.asset.as_str(),
                notional_asset.as_str()
            ));
        }
        if contract_value.value <= f64::EPSILON {
            return Err(format!(
                "contract value must be positive, got {}",
                contract_value.value
            ));
        }
        Self::try_new(
            instrument,
            base_asset,
            quote_asset,
            notional_asset,
            settlement_asset,
            PositionBasis::Contracts { contract_value },
            price_precision,
            price_tick,
            position_qty_step,
            min_position_qty,
            min_notional,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn try_new(
        instrument: Instrument,
        base_asset: Asset,
        quote_asset: Asset,
        notional_asset: Asset,
        settlement_asset: Asset,
        position_basis: PositionBasis,
        price_precision: PricePrecision,
        price_tick: f64,
        position_qty_step: f64,
        min_position_qty: f64,
        min_notional: Amount,
    ) -> Result<Self, String> {
        let spec = Self {
            instrument,
            base_asset,
            quote_asset,
            notional_asset,
            settlement_asset,
            position_basis,
            price_precision,
            price_tick,
            position_qty_step: PositionQty(position_qty_step),
            min_position_qty: PositionQty(min_position_qty),
            min_notional,
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.instrument.symbol.trim().is_empty() {
            return Err("instrument symbol must not be empty".to_string());
        }
        for (field, asset) in [
            ("base asset", &self.base_asset),
            ("quote asset", &self.quote_asset),
            ("notional asset", &self.notional_asset),
            ("settlement asset", &self.settlement_asset),
            ("minimum notional asset", self.min_notional.asset()),
        ] {
            if asset.as_str().trim().is_empty() {
                return Err(format!("{field} must not be empty"));
            }
        }
        validate_positive("price tick", self.price_tick)?;
        validate_positive("position quantity step", self.position_qty_step.0)?;
        validate_non_negative("minimum position quantity", self.min_position_qty.0)?;
        validate_non_negative("minimum notional", self.min_notional.value)?;
        if let PricePrecision::SignificantFigures {
            significant_figures,
            ..
        } = self.price_precision
            && significant_figures == 0
        {
            return Err("price significant figures must be positive".to_string());
        }
        if self.min_notional.asset != self.notional_asset {
            return Err(format!(
                "minimum notional asset `{}` does not match notional asset `{}`",
                self.min_notional.asset.as_str(),
                self.notional_asset.as_str()
            ));
        }
        if let PositionBasis::Contracts { contract_value } = &self.position_basis {
            validate_positive("contract value", contract_value.value)?;
            if contract_value.asset != self.notional_asset {
                return Err(format!(
                    "contract value asset `{}` does not match notional asset `{}`",
                    contract_value.asset.as_str(),
                    self.notional_asset.as_str()
                ));
            }
        }
        Ok(())
    }

    pub fn instrument(&self) -> &Instrument {
        &self.instrument
    }

    pub fn base_asset(&self) -> &Asset {
        &self.base_asset
    }

    pub fn quote_asset(&self) -> &Asset {
        &self.quote_asset
    }

    pub fn notional_asset(&self) -> &Asset {
        &self.notional_asset
    }

    pub fn settlement_asset(&self) -> &Asset {
        &self.settlement_asset
    }

    pub fn position_basis(&self) -> &PositionBasis {
        &self.position_basis
    }

    pub fn price_tick(&self) -> f64 {
        self.price_tick
    }

    pub fn price_precision(&self) -> PricePrecision {
        self.price_precision
    }

    pub fn position_qty_step(&self) -> PositionQty {
        self.position_qty_step
    }

    pub fn min_position_qty(&self) -> PositionQty {
        self.min_position_qty
    }

    pub fn min_notional(&self) -> &Amount {
        &self.min_notional
    }

    pub fn contract_value(&self) -> Option<&Amount> {
        match &self.position_basis {
            PositionBasis::BaseAsset => None,
            PositionBasis::Contracts { contract_value } => Some(contract_value),
        }
    }

    pub fn normalize_price(&self, price: f64, rounding: PriceRounding) -> f64 {
        self.price_precision.round(price, rounding, self.price_tick)
    }

    pub fn prices_match(&self, left: f64, right: f64) -> bool {
        match self.price_precision {
            PricePrecision::FixedTick => values_match(left, right, self.price_tick),
            _ => {
                let left = self.normalize_price(left, PriceRounding::Nearest);
                let right = self.normalize_price(right, PriceRounding::Nearest);
                values_match(
                    left,
                    right,
                    self.price_precision.match_tolerance(left, right),
                )
            }
        }
    }

    pub fn normalize_position_qty(&self, quantity: PositionQty) -> PositionQty {
        let step = self.position_qty_step.0;
        let scaled = quantity.0.abs() / step;
        let tolerance = scaled.max(1.0) * f64::EPSILON * 16.0;
        let normalized = (scaled + tolerance).floor() * step * quantity.0.signum();
        PositionQty(if normalized == -0.0 { 0.0 } else { normalized })
    }

    pub fn position_qty_for_notional(
        &self,
        notional: Amount,
        price: f64,
    ) -> Result<PositionQty, String> {
        self.ensure_notional_asset(notional.asset())?;
        let quantity = match &self.position_basis {
            PositionBasis::BaseAsset => notional.value / validate_price(price)?,
            PositionBasis::Contracts { contract_value } => notional.value / contract_value.value,
        };
        PositionQty::new(quantity)
    }

    pub fn notional_from_position_qty(
        &self,
        quantity: PositionQty,
        price: f64,
    ) -> Result<Amount, String> {
        let value = match &self.position_basis {
            PositionBasis::BaseAsset => quantity.0.abs() * validate_price(price)?,
            PositionBasis::Contracts { contract_value } => quantity.0.abs() * contract_value.value,
        };
        Amount::new(value, self.notional_asset.clone())
    }

    pub fn signed_notional_from_position_qty(
        &self,
        quantity: PositionQty,
        price: f64,
    ) -> Result<Amount, String> {
        let value = match &self.position_basis {
            PositionBasis::BaseAsset => quantity.0 * validate_price(price)?,
            PositionBasis::Contracts { contract_value } => quantity.0 * contract_value.value,
        };
        Amount::new(value, self.notional_asset.clone())
    }

    pub fn base_exposure_from_position_qty(
        &self,
        quantity: PositionQty,
        price: f64,
    ) -> Result<Amount, String> {
        let value = match &self.position_basis {
            PositionBasis::BaseAsset => quantity.0,
            PositionBasis::Contracts { contract_value } => {
                quantity.0 * contract_value.value / validate_price(price)?
            }
        };
        Amount::new(value, self.base_asset.clone())
    }

    pub fn meets_minimum(&self, quantity: PositionQty, price: f64) -> Result<bool, String> {
        Ok(quantity.0.abs() + f64::EPSILON >= self.min_position_qty.0
            && self.notional_from_position_qty(quantity, price)?.value + f64::EPSILON
                >= self.min_notional.value)
    }

    fn ensure_notional_asset(&self, asset: &Asset) -> Result<(), String> {
        if asset != &self.notional_asset {
            return Err(format!(
                "notional asset `{}` does not match instrument notional asset `{}`",
                asset.as_str(),
                self.notional_asset.as_str()
            ));
        }
        Ok(())
    }
}

fn validate_positive(name: &str, value: f64) -> Result<(), String> {
    if !value.is_finite() || value <= f64::EPSILON {
        return Err(format!("{name} must be finite and positive, got {value}"));
    }
    Ok(())
}

fn validate_non_negative(name: &str, value: f64) -> Result<(), String> {
    if !value.is_finite() || value < 0.0 {
        return Err(format!(
            "{name} must be finite and non-negative, got {value}"
        ));
    }
    Ok(())
}

fn validate_price(price: f64) -> Result<f64, String> {
    validate_positive("price", price)?;
    Ok(price)
}

fn values_match(left: f64, right: f64, tolerance: f64) -> bool {
    (left - right).abs() <= tolerance.max(f64::EPSILON) + f64::EPSILON
}

#[cfg(test)]
mod tests {
    use crate::track::{Instrument, Venue};
    use crate::types::{PricePrecision, PriceRounding};

    use super::{Amount, Asset, InstrumentSpec, PositionBasis, PositionQty};

    fn btc_usdt_linear() -> InstrumentSpec {
        InstrumentSpec::try_linear(
            Instrument::new(Venue::Okx, "BTC-USDT-SWAP"),
            Asset::try_new("BTC").unwrap(),
            Asset::try_new("USDT").unwrap(),
            Asset::try_new("USDT").unwrap(),
            PricePrecision::FixedTick,
            0.1,
            0.0001,
            0.0001,
            Amount::new(0.0, Asset::try_new("USDT").unwrap()).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn linear_contract_uses_base_asset_as_canonical_position_quantity() {
        let spec = btc_usdt_linear();

        assert_eq!(spec.position_basis(), &PositionBasis::BaseAsset);
        assert_eq!(spec.position_qty_step(), PositionQty::new(0.0001).unwrap());
        assert_eq!(spec.min_position_qty(), PositionQty::new(0.0001).unwrap());
        assert_eq!(
            spec.normalize_price(64_000.06, PriceRounding::Down),
            64_000.0
        );
        assert!(
            (spec
                .normalize_position_qty(PositionQty::new(0.03009).unwrap())
                .value()
                - 0.03)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn significant_figure_prices_do_not_match_across_adjacent_effective_steps() {
        let usdc = Asset::try_new("USDC").unwrap();
        let spec = InstrumentSpec::try_linear(
            Instrument::new(Venue::Hyperliquid, "ETH"),
            Asset::try_new("ETH").unwrap(),
            usdc.clone(),
            usdc.clone(),
            PricePrecision::significant_figures(1, 5),
            0.1,
            0.0001,
            0.0001,
            Amount::new(10.0, usdc).unwrap(),
        )
        .unwrap();

        assert!(!spec.prices_match(1_234.6, 1_234.7));
    }

    #[test]
    fn deserialized_spec_must_revalidate_its_invariants() {
        let mut value = serde_json::to_value(btc_usdt_linear()).unwrap();
        value["price_tick"] = serde_json::json!(0.0);
        let spec: InstrumentSpec = serde_json::from_value(value).unwrap();

        let error = spec.validate().unwrap_err();

        assert!(error.contains("price tick"));
        assert!(error.contains("positive"));
    }

    #[test]
    fn linear_contract_converts_notional_and_base_exposure_from_one_spec() {
        let spec = btc_usdt_linear();
        let qty = PositionQty::new(0.03).unwrap();

        assert_eq!(
            spec.notional_from_position_qty(qty, 64_000.0).unwrap(),
            Amount::new(1_920.0, Asset::try_new("USDT").unwrap()).unwrap()
        );
        assert_eq!(
            spec.base_exposure_from_position_qty(qty, 64_000.0).unwrap(),
            Amount::new(0.03, Asset::try_new("BTC").unwrap()).unwrap()
        );
        assert_eq!(
            spec.position_qty_for_notional(
                Amount::new(1_920.0, Asset::try_new("USDT").unwrap()).unwrap(),
                64_000.0,
            )
            .unwrap(),
            qty
        );
    }

    #[test]
    fn spec_rejects_invalid_or_mismatched_units() {
        let error = InstrumentSpec::try_linear(
            Instrument::new(Venue::Okx, "BTC-USDT-SWAP"),
            Asset::try_new("BTC").unwrap(),
            Asset::try_new("USDT").unwrap(),
            Asset::try_new("USDT").unwrap(),
            PricePrecision::FixedTick,
            0.0,
            0.0001,
            0.0001,
            Amount::new(0.0, Asset::try_new("BTC").unwrap()).unwrap(),
        )
        .unwrap_err();

        assert!(error.contains("price tick"));
    }
}
