use std::collections::HashMap;
use std::sync::RwLock;

use anyhow::{Context, Result, anyhow, ensure};

use poise_core::track::{Instrument, Venue};
use poise_core::types::{ExchangeRules, QuantityKind};
use poise_engine::ports::ExchangeInfo;

use crate::rest::models::InstrumentInfo;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OkxInstrumentMetadata {
    symbol: String,
    contract_kind: OkxContractKind,
    ct_val: Option<f64>,
    ct_val_ccy: Option<String>,
    settle_ccy: String,
}

impl OkxInstrumentMetadata {
    pub(crate) fn from_instrument_info(value: &InstrumentInfo) -> Result<Self> {
        let instrument = Instrument::new(Venue::Okx, value.inst_id.clone());
        let explicit_settle_ccy = value
            .settle_ccy
            .as_deref()
            .filter(|asset| !asset.trim().is_empty())
            .map(ToString::to_string);
        let ct_val = value
            .ct_val
            .as_deref()
            .map(|raw| parse_decimal("ctVal", raw))
            .transpose()?;
        let ct_val_ccy = value
            .ct_val_ccy
            .as_deref()
            .filter(|asset| !asset.trim().is_empty())
            .map(ToString::to_string);
        let contract_kind = match value.ct_type.as_deref() {
            Some("inverse") => {
                ensure!(
                    ct_val.unwrap_or_default() > 0.0,
                    "OKX inverse instrument `{}` missing positive ctVal",
                    value.inst_id
                );
                ensure!(
                    ct_val_ccy.as_deref() == Some("USD"),
                    "OKX inverse instrument `{}` ctValCcy must be USD",
                    value.inst_id
                );
                ensure!(
                    explicit_settle_ccy.is_some(),
                    "OKX inverse instrument `{}` missing settleCcy",
                    value.inst_id
                );
                OkxContractKind::Inverse
            }
            Some("linear") => {
                if let Some(ct_val) = ct_val {
                    ensure!(
                        ct_val > 0.0,
                        "OKX linear instrument `{}` has non-positive ctVal",
                        value.inst_id
                    );
                    let base_asset = okx_base_asset(&value.inst_id);
                    ensure!(
                        ct_val_ccy.as_deref() == Some(base_asset),
                        "OKX linear instrument `{}` ctValCcy must be base asset `{base_asset}`",
                        value.inst_id
                    );
                }
                OkxContractKind::Linear
            }
            Some(other) => return Err(anyhow!("unsupported OKX ctType `{other}`")),
            None => OkxContractKind::Linear,
        };
        let settle_ccy = explicit_settle_ccy.unwrap_or_else(|| instrument.quote_asset());

        Ok(Self {
            symbol: value.inst_id.clone(),
            contract_kind,
            ct_val,
            ct_val_ccy,
            settle_ccy,
        })
    }

    pub(crate) fn symbol(&self) -> &str {
        &self.symbol
    }

    pub(crate) fn settlement_asset(&self) -> &str {
        &self.settle_ccy
    }

    pub(crate) fn exchange_info_from_instrument(
        &self,
        value: &InstrumentInfo,
    ) -> Result<ExchangeInfo> {
        let contract_size = self.linear_contract_size();
        let (quantity_kind, contract_notional, quantity_step, min_qty) = match self.contract_kind {
            OkxContractKind::Inverse => (
                QuantityKind::InverseContract,
                self.ct_val,
                parse_decimal("lotSz", &value.lot_sz)?,
                parse_decimal("minSz", &value.min_sz)?,
            ),
            OkxContractKind::Linear => (
                QuantityKind::BaseAsset,
                None,
                parse_decimal("lotSz", &value.lot_sz)? * contract_size,
                parse_decimal("minSz", &value.min_sz)? * contract_size,
            ),
        };

        Ok(ExchangeInfo {
            instrument: Instrument::new(Venue::Okx, value.inst_id.clone()),
            rules: ExchangeRules {
                price_tick: parse_decimal("tickSz", &value.tick_sz)?,
                price_precision: Default::default(),
                quantity_kind,
                contract_notional,
                settlement_asset: self.settle_ccy.clone(),
                quantity_step,
                min_qty,
                min_notional: 0.0,
                maker_fee_rate: 0.0002,
                taker_fee_rate: 0.0005,
            },
        })
    }

    pub(crate) fn okx_contract_qty_from_native(&self, native_qty: f64) -> f64 {
        match self.contract_kind {
            OkxContractKind::Inverse => native_qty,
            OkxContractKind::Linear => native_qty / self.linear_contract_size(),
        }
    }

    pub(crate) fn native_qty_from_okx_contracts(&self, okx_contract_qty: f64) -> f64 {
        match self.contract_kind {
            OkxContractKind::Inverse => okx_contract_qty,
            OkxContractKind::Linear => okx_contract_qty * self.linear_contract_size(),
        }
    }

    fn linear_contract_size(&self) -> f64 {
        self.ct_val.unwrap_or(1.0)
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

fn okx_base_asset(symbol: &str) -> &str {
    symbol.split('-').next().unwrap_or(symbol)
}

fn parse_decimal(field: &str, value: &str) -> Result<f64> {
    value
        .parse::<f64>()
        .with_context(|| format!("invalid decimal for {field}: {value}"))
}
