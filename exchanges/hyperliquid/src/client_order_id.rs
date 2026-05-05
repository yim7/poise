use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use sha3::{Digest, Keccak256};

#[derive(Debug, Default)]
pub(crate) struct ClientOrderIdMapper {
    exchange_to_local: Mutex<HashMap<String, String>>,
}

impl ClientOrderIdMapper {
    pub(crate) fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn exchange_id_for_local(&self, local_client_order_id: &str) -> String {
        if is_hyperliquid_cloid(local_client_order_id) {
            return local_client_order_id.to_string();
        }

        let exchange_client_order_id = hyperliquid_cloid(local_client_order_id);
        self.exchange_to_local.lock().unwrap().insert(
            exchange_client_order_id.clone(),
            local_client_order_id.to_string(),
        );
        exchange_client_order_id
    }

    pub(crate) fn local_id_for_exchange(&self, exchange_client_order_id: &str) -> String {
        self.exchange_to_local
            .lock()
            .unwrap()
            .get(exchange_client_order_id)
            .cloned()
            .unwrap_or_else(|| exchange_client_order_id.to_string())
    }
}

pub(crate) fn hyperliquid_cloid(client_order_id: &str) -> String {
    if is_hyperliquid_cloid(client_order_id) {
        return client_order_id.to_string();
    }

    let mut hasher = Keccak256::new();
    hasher.update(client_order_id.as_bytes());
    let digest = hasher.finalize();
    format!("0x{}", hex::encode(&digest[..16]))
}

fn is_hyperliquid_cloid(value: &str) -> bool {
    value.len() == 34
        && value.starts_with("0x")
        && value[2..].chars().all(|ch| ch.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{ClientOrderIdMapper, hyperliquid_cloid};

    #[test]
    fn maps_internal_id_to_hyperliquid_cloid_and_back_within_process() {
        let mapper = ClientOrderIdMapper::default();
        let local_id = "bk-56961625d79c44978c760c53fda4eefc";

        let exchange_id = mapper.exchange_id_for_local(local_id);

        assert_ne!(exchange_id, local_id);
        assert_eq!(exchange_id.len(), 34);
        assert_eq!(mapper.local_id_for_exchange(&exchange_id), local_id);
    }

    #[test]
    fn leaves_external_cloid_unchanged_when_mapping_is_unknown() {
        let mapper = ClientOrderIdMapper::default();
        let exchange_id = "0x11111111111111111111111111111111";

        assert_eq!(mapper.exchange_id_for_local(exchange_id), exchange_id);
        assert_eq!(mapper.local_id_for_exchange(exchange_id), exchange_id);
    }

    #[test]
    fn hyperliquid_cloid_is_deterministic() {
        assert_eq!(
            hyperliquid_cloid("bk-56961625d79c44978c760c53fda4eefc"),
            hyperliquid_cloid("bk-56961625d79c44978c760c53fda4eefc")
        );
    }
}
