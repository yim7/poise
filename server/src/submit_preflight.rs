use std::collections::HashSet;

use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitPreflightDecision {
    Direct,
    NeedsLiveOrderLookup { client_order_id: String },
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

    pub async fn decide(&self, effect_id: &str, client_order_id: &str) -> SubmitPreflightDecision {
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

    pub async fn seed_startup_pending_submit_effects(
        &self,
        effect_ids: impl IntoIterator<Item = String>,
    ) {
        let mut startup_pending = self.startup_pending_submit_effects.lock().await;
        startup_pending.clear();
        startup_pending.extend(effect_ids);
    }

    #[cfg(test)]
    pub async fn startup_pending_effect_ids(&self) -> HashSet<String> {
        self.startup_pending_submit_effects.lock().await.clone()
    }

    #[cfg(test)]
    pub async fn is_attempted(&self, effect_id: &str) -> bool {
        self.attempted_submit_effects.lock().await.contains(effect_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{SubmitPreflight, SubmitPreflightDecision};

    #[tokio::test]
    async fn submit_preflight_decides_direct_for_fresh_effect() {
        let preflight = SubmitPreflight::new();

        let decision = preflight.decide("effect-1", "client-1").await;

        assert_eq!(decision, SubmitPreflightDecision::Direct);
    }

    #[tokio::test]
    async fn submit_preflight_decides_lookup_for_started_effect() {
        let preflight = SubmitPreflight::new();
        preflight.mark_submit_started("effect-1").await;

        let decision = preflight.decide("effect-1", "client-1").await;

        assert_eq!(
            decision,
            SubmitPreflightDecision::NeedsLiveOrderLookup {
                client_order_id: "client-1".into()
            }
        );
    }

    #[tokio::test]
    async fn submit_preflight_decides_lookup_for_startup_pending_effect() {
        let preflight = SubmitPreflight::new();
        preflight
            .seed_startup_pending_submit_effects(["effect-1".to_string()])
            .await;

        let decision = preflight.decide("effect-1", "client-1").await;

        assert_eq!(
            decision,
            SubmitPreflightDecision::NeedsLiveOrderLookup {
                client_order_id: "client-1".into()
            }
        );
    }
}
