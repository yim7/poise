use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, Notify};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitPreflightDecision {
    Direct,
    NeedsLiveOrderLookup { client_order_id: String },
}

pub struct SubmitPreflight {
    attempted_submit_effects: Mutex<HashSet<String>>,
    startup_pending_submit_effects: Mutex<HashSet<String>>,
    pending_submit_effects_dirty: AtomicBool,
    pending_submit_effects_notify: Notify,
}

impl Default for SubmitPreflight {
    fn default() -> Self {
        Self {
            attempted_submit_effects: Mutex::new(HashSet::new()),
            startup_pending_submit_effects: Mutex::new(HashSet::new()),
            pending_submit_effects_dirty: AtomicBool::new(false),
            pending_submit_effects_notify: Notify::new(),
        }
    }
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

    pub async fn has_tracked_submit_effects(&self) -> bool {
        if !self.startup_pending_submit_effects.lock().await.is_empty() {
            return true;
        }
        !self.attempted_submit_effects.lock().await.is_empty()
    }

    pub fn mark_pending_submit_effects_dirty(&self) {
        self.pending_submit_effects_dirty
            .store(true, Ordering::SeqCst);
        self.pending_submit_effects_notify.notify_one();
    }

    pub fn take_pending_submit_effects_dirty(&self) -> bool {
        self.pending_submit_effects_dirty
            .swap(false, Ordering::SeqCst)
    }

    pub async fn wait_for_pending_submit_effects_dirty(&self) {
        self.pending_submit_effects_notify.notified().await;
    }

    #[cfg(test)]
    pub async fn startup_pending_effect_ids(&self) -> HashSet<String> {
        self.startup_pending_submit_effects.lock().await.clone()
    }

    #[cfg(test)]
    pub async fn is_attempted(&self, effect_id: &str) -> bool {
        self.attempted_submit_effects
            .lock()
            .await
            .contains(effect_id)
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

    #[tokio::test]
    async fn submit_preflight_dirty_flag_coalesces_multiple_marks_until_taken() {
        let preflight = SubmitPreflight::new();

        assert!(!preflight.take_pending_submit_effects_dirty());

        preflight.mark_pending_submit_effects_dirty();
        preflight.mark_pending_submit_effects_dirty();

        assert!(preflight.take_pending_submit_effects_dirty());
        assert!(!preflight.take_pending_submit_effects_dirty());
    }
}
