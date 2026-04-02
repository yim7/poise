use std::collections::HashSet;

use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitPreflightDecision {
    Direct,
    NeedsLiveOrderLookup { client_order_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitPreflightHint {
    DirectSafe,
    NeedsExchangeStateLookup,
}

#[derive(Default)]
pub struct SubmitPreflight {
    attempted_submit_effects: Mutex<HashSet<String>>,
    startup_pending_submit_effects: Mutex<HashSet<String>>,
}

impl SubmitPreflight {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn decide(
        &self,
        effect_id: &str,
        client_order_id: &str,
        hint: SubmitPreflightHint,
    ) -> SubmitPreflightDecision {
        if matches!(hint, SubmitPreflightHint::NeedsExchangeStateLookup) {
            return SubmitPreflightDecision::NeedsLiveOrderLookup {
                client_order_id: client_order_id.to_string(),
            };
        }

        if self
            .startup_pending_submit_effects
            .lock()
            .await
            .contains(effect_id)
        {
            return SubmitPreflightDecision::NeedsLiveOrderLookup {
                client_order_id: client_order_id.to_string(),
            };
        }

        if self
            .attempted_submit_effects
            .lock()
            .await
            .contains(effect_id)
        {
            return SubmitPreflightDecision::NeedsLiveOrderLookup {
                client_order_id: client_order_id.to_string(),
            };
        }

        SubmitPreflightDecision::Direct
    }

    pub async fn mark_submit_started(&self, effect_id: &str) {
        self.attempted_submit_effects
            .lock()
            .await
            .insert(effect_id.to_string());
    }

    pub async fn reconcile_pending_submit_effects(&self, current: &HashSet<String>) {
        self.startup_pending_submit_effects
            .lock()
            .await
            .retain(|effect_id| current.contains(effect_id));
        self.attempted_submit_effects
            .lock()
            .await
            .retain(|effect_id| current.contains(effect_id));
    }

    pub async fn submit_started_effects(&self) -> HashSet<String> {
        self.attempted_submit_effects.lock().await.clone()
    }

    #[cfg(test)]
    pub async fn is_attempted(&self, effect_id: &str) -> bool {
        self.attempted_submit_effects.lock().await.contains(effect_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{SubmitPreflight, SubmitPreflightDecision, SubmitPreflightHint};

    #[tokio::test]
    async fn submit_preflight_decides_direct_for_fresh_effect() {
        let preflight = SubmitPreflight::new();

        let decision = preflight
            .decide("effect-1", "client-1", SubmitPreflightHint::DirectSafe)
            .await;

        assert_eq!(decision, SubmitPreflightDecision::Direct);
    }

    #[tokio::test]
    async fn submit_preflight_decides_lookup_for_started_effect() {
        let preflight = SubmitPreflight::new();
        preflight.mark_submit_started("effect-1").await;

        let decision = preflight
            .decide("effect-1", "client-1", SubmitPreflightHint::DirectSafe)
            .await;

        assert_eq!(
            decision,
            SubmitPreflightDecision::NeedsLiveOrderLookup {
                client_order_id: "client-1".into()
            }
        );
    }
}
