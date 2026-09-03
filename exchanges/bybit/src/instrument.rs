use std::collections::HashMap;
use std::sync::RwLock;

use poise_core::instrument::InstrumentSpec;

#[derive(Debug, Default)]
pub(crate) struct BybitInstrumentRegistry {
    specs_by_symbol: RwLock<HashMap<String, InstrumentSpec>>,
}

impl BybitInstrumentRegistry {
    pub(crate) fn upsert(&self, spec: InstrumentSpec) {
        self.specs_by_symbol
            .write()
            .expect("Bybit instrument registry poisoned")
            .insert(spec.instrument().symbol.clone(), spec);
    }

    pub(crate) fn get(&self, symbol: &str) -> Option<InstrumentSpec> {
        self.specs_by_symbol
            .read()
            .expect("Bybit instrument registry poisoned")
            .get(symbol)
            .cloned()
    }
}
