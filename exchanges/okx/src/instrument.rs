use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::{Context, Result, anyhow, ensure};

use poise_core::instrument::{Amount, Asset, InstrumentSpec, PositionQty};
use poise_core::track::{Instrument, Venue};
use poise_core::types::{ExchangeRules, QuantityKind};
use poise_engine::ports::ExchangeInfo;

use crate::rest::models::InstrumentInfo;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OkxInstrumentMetadata {
    contract_kind: OkxContractKind,
    ct_val: f64,
    instrument_spec: InstrumentSpec,
}

impl OkxInstrumentMetadata {
    pub(crate) fn from_instrument_info(value: &InstrumentInfo) -> Result<Self> {
        let instrument = Instrument::new(Venue::Okx, value.inst_id.clone());
        let settle_ccy = value
            .settle_ccy
            .as_deref()
            .filter(|asset| !asset.trim().is_empty())
            .context("missing settleCcy")?;
        let ct_val = value
            .ct_val
            .as_deref()
            .filter(|raw| !raw.trim().is_empty())
            .context("missing ctVal")
            .and_then(|raw| parse_decimal("ctVal", raw))?;
        let ct_val_ccy = value
            .ct_val_ccy
            .as_deref()
            .filter(|asset| !asset.trim().is_empty())
            .context("missing ctValCcy")?;
        let tick_size = parse_decimal("tickSz", &value.tick_sz)?;
        let lot_size = parse_decimal("lotSz", &value.lot_sz)?;
        let min_size = parse_decimal("minSz", &value.min_sz)?;
        ensure!(
            tick_size.is_finite() && tick_size > f64::EPSILON,
            "OKX instrument `{}` tickSz must be positive",
            value.inst_id
        );
        ensure!(
            lot_size.is_finite() && lot_size > f64::EPSILON,
            "OKX instrument `{}` lotSz must be positive",
            value.inst_id
        );
        ensure!(
            min_size.is_finite() && min_size >= 0.0,
            "OKX instrument `{}` minSz must be non-negative",
            value.inst_id
        );
        let (base_asset, quote_asset) = okx_symbol_assets(&value.inst_id)?;
        let base_asset = Asset::try_new(base_asset).map_err(anyhow::Error::msg)?;
        let quote_asset = Asset::try_new(quote_asset).map_err(anyhow::Error::msg)?;
        let settlement_asset = Asset::try_new(settle_ccy).map_err(anyhow::Error::msg)?;
        let contract_kind = match value.ct_type.as_deref() {
            Some("inverse") => {
                ensure!(
                    ct_val > 0.0,
                    "OKX inverse instrument `{}` missing positive ctVal",
                    value.inst_id
                );
                ensure!(
                    ct_val_ccy == "USD",
                    "OKX inverse instrument `{}` ctValCcy must be USD",
                    value.inst_id
                );
                OkxContractKind::Inverse
            }
            Some("linear") => {
                ensure!(
                    ct_val > 0.0,
                    "OKX linear instrument `{}` has non-positive ctVal",
                    value.inst_id
                );
                ensure!(
                    ct_val_ccy == base_asset.as_str(),
                    "OKX linear instrument `{}` ctValCcy must be base asset `{}`",
                    value.inst_id,
                    base_asset.as_str()
                );
                OkxContractKind::Linear
            }
            Some(other) => return Err(anyhow!("unsupported OKX ctType `{other}`")),
            None => return Err(anyhow!("OKX instrument `{}` missing ctType", value.inst_id)),
        };
        let min_notional = Amount::new(0.0, quote_asset.clone()).map_err(anyhow::Error::msg)?;
        let instrument_spec = match contract_kind {
            OkxContractKind::Linear => InstrumentSpec::try_linear(
                instrument,
                base_asset,
                quote_asset,
                settlement_asset,
                Default::default(),
                tick_size,
                lot_size * ct_val,
                min_size * ct_val,
                min_notional,
            ),
            OkxContractKind::Inverse => InstrumentSpec::try_inverse(
                instrument,
                base_asset,
                quote_asset,
                settlement_asset,
                Amount::new(
                    ct_val,
                    Asset::try_new(ct_val_ccy).map_err(anyhow::Error::msg)?,
                )
                .map_err(anyhow::Error::msg)?,
                Default::default(),
                tick_size,
                lot_size,
                min_size,
                min_notional,
            ),
        }
        .map_err(|error| {
            anyhow!(
                "invalid OKX instrument metadata for `{}`: {error}",
                value.inst_id
            )
        })?;

        Ok(Self {
            contract_kind,
            ct_val,
            instrument_spec,
        })
    }

    pub(crate) fn symbol(&self) -> &str {
        &self.instrument_spec.instrument().symbol
    }

    pub(crate) fn settlement_asset(&self) -> &str {
        self.instrument_spec.settlement_asset().as_str()
    }

    pub(crate) fn instrument_spec(&self) -> &InstrumentSpec {
        &self.instrument_spec
    }

    pub(crate) fn is_inverse(&self) -> bool {
        matches!(self.contract_kind, OkxContractKind::Inverse)
    }

    pub(crate) fn contract_notional(&self) -> Option<f64> {
        match self.contract_kind {
            OkxContractKind::Inverse => Some(self.ct_val),
            OkxContractKind::Linear => None,
        }
    }

    pub(crate) fn exchange_info_from_instrument(
        &self,
        value: &InstrumentInfo,
    ) -> Result<ExchangeInfo> {
        let (quantity_kind, contract_notional, quantity_step, min_qty) = match self.contract_kind {
            OkxContractKind::Inverse => (
                QuantityKind::InverseContract,
                Some(self.ct_val),
                self.instrument_spec.position_qty_step().value(),
                self.instrument_spec.min_position_qty().value(),
            ),
            OkxContractKind::Linear => (
                QuantityKind::BaseAsset,
                None,
                self.instrument_spec.position_qty_step().value(),
                self.instrument_spec.min_position_qty().value(),
            ),
        };

        Ok(ExchangeInfo {
            instrument: Instrument::new(Venue::Okx, value.inst_id.clone()),
            rules: ExchangeRules {
                price_tick: self.instrument_spec.price_tick(),
                price_precision: self.instrument_spec.price_precision(),
                quantity_kind,
                contract_notional,
                settlement_asset: self.settlement_asset().to_string(),
                quantity_step,
                min_qty,
                min_notional: self.instrument_spec.min_notional().value(),
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.0005,
            },
        })
    }

    pub(crate) fn wire_contracts_from_position_qty(
        &self,
        position_qty: PositionQty,
    ) -> Result<f64> {
        let normalized = self
            .instrument_spec
            .normalize_position_qty(position_qty)
            .value();
        let tolerance = self.instrument_spec.position_qty_step().value() * 1e-9;
        ensure!(
            (normalized - position_qty.value()).abs() <= tolerance.max(f64::EPSILON),
            "canonical position quantity {} is not normalized to step {} for `{}`",
            position_qty.value(),
            self.instrument_spec.position_qty_step().value(),
            self.symbol()
        );

        let wire_contracts = match self.contract_kind {
            OkxContractKind::Inverse => position_qty.value(),
            OkxContractKind::Linear => position_qty.value() / self.linear_contract_size(),
        };
        ensure!(
            wire_contracts.is_finite(),
            "wire contract quantity for `{}` must be finite",
            self.symbol()
        );
        Ok(wire_contracts)
    }

    pub(crate) fn position_qty_from_wire_contracts(
        &self,
        wire_contracts: f64,
    ) -> Result<PositionQty> {
        ensure!(
            wire_contracts.is_finite(),
            "wire contract quantity for `{}` must be finite",
            self.symbol()
        );
        let position_qty = match self.contract_kind {
            OkxContractKind::Inverse => wire_contracts,
            OkxContractKind::Linear => wire_contracts * self.linear_contract_size(),
        };
        PositionQty::new(position_qty).map_err(anyhow::Error::msg)
    }

    fn linear_contract_size(&self) -> f64 {
        self.ct_val
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OkxContractKind {
    Linear,
    Inverse,
}

#[derive(Debug, Default)]
pub(crate) struct OkxInstrumentRegistry {
    metadata_by_symbol: RwLock<HashMap<String, OkxInstrumentMetadata>>,
}

impl OkxInstrumentRegistry {
    pub(crate) fn upsert(&self, metadata: OkxInstrumentMetadata) {
        self.metadata_by_symbol
            .write()
            .expect("OKX instrument registry poisoned")
            .insert(metadata.symbol().to_string(), metadata);
    }

    pub(crate) fn get(&self, symbol: &str) -> Option<OkxInstrumentMetadata> {
        self.metadata_by_symbol
            .read()
            .expect("OKX instrument registry poisoned")
            .get(symbol)
            .cloned()
    }
}

#[cfg(test)]
pub(crate) fn exchange_info_from_instrument(value: InstrumentInfo) -> Result<ExchangeInfo> {
    OkxInstrumentMetadata::from_instrument_info(&value)?.exchange_info_from_instrument(&value)
}

fn okx_symbol_assets(symbol: &str) -> Result<(&str, &str)> {
    let mut parts = symbol.split('-');
    let base_asset = parts.next().filter(|part| !part.is_empty());
    let quote_asset = parts.next().filter(|part| !part.is_empty());
    match (base_asset, quote_asset) {
        (Some(base_asset), Some(quote_asset)) => Ok((base_asset, quote_asset)),
        _ => Err(anyhow!("invalid OKX instrument symbol `{symbol}`")),
    }
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}

#[cfg(test)]
mod tests {
    use poise_core::instrument::{Asset, PositionBasis, PositionQty};

    use super::*;

    fn linear_instrument() -> InstrumentInfo {
        InstrumentInfo {
            inst_id: "BTC-USDT-SWAP".to_string(),
            ct_type: Some("linear".to_string()),
            tick_sz: "0.1".to_string(),
            lot_sz: "1".to_string(),
            min_sz: "1".to_string(),
            ct_val: Some("0.01".to_string()),
            ct_val_ccy: Some("BTC".to_string()),
            settle_ccy: Some("USDT".to_string()),
        }
    }

    #[test]
    fn linear_metadata_builds_complete_canonical_instrument_spec() {
        let metadata = OkxInstrumentMetadata::from_instrument_info(&linear_instrument()).unwrap();
        let spec = metadata.instrument_spec();

        assert_eq!(spec.instrument().symbol, "BTC-USDT-SWAP");
        assert_eq!(spec.base_asset(), &Asset::try_new("BTC").unwrap());
        assert_eq!(spec.quote_asset(), &Asset::try_new("USDT").unwrap());
        assert_eq!(spec.notional_asset(), &Asset::try_new("USDT").unwrap());
        assert_eq!(spec.settlement_asset(), &Asset::try_new("USDT").unwrap());
        assert_eq!(spec.position_basis(), &PositionBasis::BaseAsset);
        assert_eq!(spec.price_tick(), 0.1);
        assert_eq!(spec.position_qty_step(), PositionQty::new(0.01).unwrap());
        assert_eq!(spec.min_position_qty(), PositionQty::new(0.01).unwrap());
        assert_eq!(spec.min_notional().value(), 0.0);
    }

    #[test]
    fn linear_wire_contracts_round_trip_canonical_position_quantity() {
        let metadata = OkxInstrumentMetadata::from_instrument_info(&linear_instrument()).unwrap();
        let canonical = PositionQty::new(0.03).unwrap();

        let wire_contracts = metadata
            .wire_contracts_from_position_qty(canonical)
            .unwrap();
        let round_trip = metadata
            .position_qty_from_wire_contracts(wire_contracts)
            .unwrap();

        assert_eq!(wire_contracts, 3.0);
        assert!((round_trip.value() - canonical.value()).abs() < 1e-12);
    }

    #[test]
    fn linear_wire_conversion_rejects_non_normalized_quantity() {
        let metadata = OkxInstrumentMetadata::from_instrument_info(&linear_instrument()).unwrap();

        let error = metadata
            .wire_contracts_from_position_qty(PositionQty::new(0.030_001).unwrap())
            .unwrap_err();

        assert!(error.to_string().contains("not normalized"));
    }

    #[test]
    fn linear_metadata_requires_contract_value_and_units() {
        let mut instrument = linear_instrument();
        instrument.ct_val = None;
        assert!(
            OkxInstrumentMetadata::from_instrument_info(&instrument)
                .unwrap_err()
                .to_string()
                .contains("ctVal")
        );

        let mut instrument = linear_instrument();
        instrument.ct_val_ccy = Some("USDT".to_string());
        assert!(
            OkxInstrumentMetadata::from_instrument_info(&instrument)
                .unwrap_err()
                .to_string()
                .contains("base asset")
        );

        let mut instrument = linear_instrument();
        instrument.settle_ccy = None;
        assert!(
            OkxInstrumentMetadata::from_instrument_info(&instrument)
                .unwrap_err()
                .to_string()
                .contains("settleCcy")
        );
    }

    #[test]
    fn linear_metadata_rejects_invalid_tick_lot_and_minimum_quantity() {
        for (field, value) in [("tickSz", "0"), ("lotSz", "0"), ("minSz", "-1")] {
            let mut instrument = linear_instrument();
            match field {
                "tickSz" => instrument.tick_sz = value.to_string(),
                "lotSz" => instrument.lot_sz = value.to_string(),
                "minSz" => instrument.min_sz = value.to_string(),
                _ => unreachable!(),
            }

            let error = OkxInstrumentMetadata::from_instrument_info(&instrument).unwrap_err();
            assert!(
                error.to_string().contains(field),
                "expected {field} in error, got {error}"
            );
        }
    }
}
