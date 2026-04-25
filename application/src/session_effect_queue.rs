use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use poise_engine::executor::PendingSubmitHint;
use poise_engine::track::TrackId;
use poise_engine::transition::TrackEffect;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionTrackEffect {
    pub effect_id: String,
    pub track_id: TrackId,
    pub batch_id: String,
    pub sequence: u32,
    pub effect: TrackEffect,
    pub created_at: DateTime<Utc>,
}

impl SessionTrackEffect {
    pub fn from_transition_effects(
        track_id: &TrackId,
        batch_id: &str,
        effects: &[TrackEffect],
        created_at: DateTime<Utc>,
    ) -> Vec<Self> {
        effects
            .iter()
            .enumerate()
            .filter_map(|(sequence, effect)| {
                if matches!(effect, TrackEffect::NoOp) {
                    return None;
                }
                Some(Self {
                    effect_id: format!("{}:{batch_id}:{sequence}", track_id.as_str()),
                    track_id: track_id.clone(),
                    batch_id: batch_id.to_string(),
                    sequence: sequence as u32,
                    effect: effect.clone(),
                    created_at,
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionEffectQueueSnapshot {
    pub track_id: TrackId,
    pub pending_effects: Vec<SessionPendingEffectView>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionPendingEffectView {
    pub effect_id: String,
    pub kind: SessionPendingEffectKind,
    pub state: SessionPendingEffectState,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPendingEffectKind {
    Submit,
    Cancel,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPendingEffectState {
    Queued,
    InFlight,
    SubmittedAwaitingWriteback,
    Deferred { until: DeferredUntil },
    AwaitingFollowUp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEffectOutcome {
    Finished,
    Superseded,
    Deferred { until: DeferredUntil },
    Blocked { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredUntil {
    FreshMarket,
    ExchangeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeSignal {
    FreshMarket,
    ExchangeState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CancelReceiptResolution {
    ClosedWithoutFill,
    ClosedWithFill { filled_qty: f64 },
    StillWorking,
    Unknown { order_id: String, reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CancelQueueAction {
    UnblockedDownstream,
    SupersededDownstream {
        effect_ids: Vec<String>,
        requires_reconcile: bool,
    },
    Deferred {
        until: DeferredUntil,
    },
    AwaitingFollowUpRetirement {
        reason: String,
        token: FollowUpRetirementToken,
    },
    Blocked {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FollowUpRetirementToken(String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowUpRetirementResolution {
    pub token: FollowUpRetirementToken,
    pub closed_order_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FollowUpQueueAction {
    SupersededDownstream {
        effect_ids: Vec<String>,
        requires_reconcile: bool,
    },
    NothingToRetire,
    Blocked {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionQueueAction {
    Continue,
    RetiredBatch {
        effect_ids: Vec<String>,
        requires_reconcile: bool,
    },
}

#[derive(Clone, Default)]
pub struct SessionEffectQueue {
    inner: Arc<Mutex<SessionEffectQueueInner>>,
}

#[derive(Default)]
struct SessionEffectQueueInner {
    tracks: HashMap<TrackId, TrackQueue>,
    ready_tracks: VecDeque<TrackId>,
    effect_index: HashMap<String, TrackId>,
    follow_up_tokens: HashMap<FollowUpRetirementToken, FollowUpPointer>,
    next_follow_up_token: u64,
}

#[derive(Default)]
struct TrackQueue {
    batches: VecDeque<SessionEffectBatch>,
    paused_until: Option<DeferredUntil>,
    in_ready_ring: bool,
}

struct SessionEffectBatch {
    batch_id: String,
    effects: VecDeque<QueuedEffect>,
}

struct QueuedEffect {
    effect: SessionTrackEffect,
    dispatch_state: QueuedEffectState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuedEffectState {
    Queued,
    InFlight,
    SubmittedAwaitingWriteback,
    AwaitingFollowUp,
}

struct FollowUpPointer {
    track_id: TrackId,
    cancel_effect_id: String,
    closed_order_id: String,
}

impl SessionEffectQueue {
    pub fn enqueue_batch(&self, effects: Vec<SessionTrackEffect>) {
        let mut inner = self.inner.lock().unwrap();
        let mut grouped: HashMap<TrackId, Vec<SessionTrackEffect>> = HashMap::new();
        for effect in effects {
            grouped
                .entry(effect.track_id.clone())
                .or_default()
                .push(effect);
        }

        for (track_id, mut effects) in grouped {
            effects.sort_by_key(|effect| effect.sequence);
            let Some(batch_id) = effects.first().map(|effect| effect.batch_id.clone()) else {
                continue;
            };
            let queued_effects = effects
                .into_iter()
                .map(|effect| {
                    inner
                        .effect_index
                        .insert(effect.effect_id.clone(), track_id.clone());
                    QueuedEffect {
                        effect,
                        dispatch_state: QueuedEffectState::Queued,
                    }
                })
                .collect();

            let track = inner.tracks.entry(track_id.clone()).or_default();
            track.batches.push_back(SessionEffectBatch {
                batch_id,
                effects: queued_effects,
            });
            inner.mark_track_ready(&track_id);
        }
    }

    pub fn claim_next(&self) -> Option<SessionTrackEffect> {
        let mut inner = self.inner.lock().unwrap();
        let ready_len = inner.ready_tracks.len();
        for _ in 0..ready_len {
            let Some(track_id) = inner.ready_tracks.pop_front() else {
                break;
            };
            if let Some(track) = inner.tracks.get_mut(&track_id) {
                track.in_ready_ring = false;
                if track.paused_until.is_some() {
                    continue;
                }
                let Some(effect) = track.front_effect_mut() else {
                    inner.remove_empty_track(&track_id);
                    continue;
                };
                if effect.dispatch_state == QueuedEffectState::Queued {
                    effect.dispatch_state = QueuedEffectState::InFlight;
                    return Some(effect.effect.clone());
                }
            }
        }
        None
    }

    pub fn record_submit_exchange_accepted(&self, effect_id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some((track_id, effect)) = inner.effect_mut(effect_id) else {
            return false;
        };
        if !matches!(effect.effect.effect, TrackEffect::SubmitOrder { .. })
            || effect.dispatch_state != QueuedEffectState::InFlight
        {
            return false;
        }
        effect.dispatch_state = QueuedEffectState::SubmittedAwaitingWriteback;
        inner.mark_track_ready(&track_id);
        true
    }

    pub fn wake_track_for(&self, track_id: &TrackId, signal: WakeSignal) {
        let mut inner = self.inner.lock().unwrap();
        let Some(track) = inner.tracks.get_mut(track_id) else {
            return;
        };
        let Some(paused_until) = track.paused_until else {
            inner.mark_track_ready(track_id);
            return;
        };
        if paused_until.matches(signal) {
            track.paused_until = None;
            inner.mark_track_ready(track_id);
        }
    }

    pub fn record_outcome(
        &self,
        effect_id: &str,
        outcome: SessionEffectOutcome,
    ) -> SessionQueueAction {
        let mut inner = self.inner.lock().unwrap();
        let Some(track_id) = inner.effect_index.get(effect_id).cloned() else {
            return SessionQueueAction::Continue;
        };

        match outcome {
            SessionEffectOutcome::Finished | SessionEffectOutcome::Superseded => {
                inner.remove_effect(&track_id, effect_id);
                inner.mark_track_ready(&track_id);
                SessionQueueAction::Continue
            }
            SessionEffectOutcome::Deferred { until } => {
                if let Some(track) = inner.tracks.get_mut(&track_id) {
                    track.paused_until = Some(until);
                    if let Some(effect) = track.front_effect_mut()
                        && effect.effect.effect_id == effect_id
                        && effect.dispatch_state != QueuedEffectState::SubmittedAwaitingWriteback
                    {
                        effect.dispatch_state = QueuedEffectState::Queued;
                    }
                }
                SessionQueueAction::Continue
            }
            SessionEffectOutcome::Blocked { .. } => {
                let retired = inner.retire_current_batch_after(&track_id, effect_id);
                inner.mark_track_ready(&track_id);
                SessionQueueAction::RetiredBatch {
                    effect_ids: retired,
                    requires_reconcile: true,
                }
            }
        }
    }

    pub fn record_cancel_resolution(
        &self,
        effect_id: &str,
        resolution: CancelReceiptResolution,
    ) -> CancelQueueAction {
        let mut inner = self.inner.lock().unwrap();
        let Some(track_id) = inner.effect_index.get(effect_id).cloned() else {
            return CancelQueueAction::Blocked {
                reason: format!("effect `{effect_id}` not found"),
            };
        };

        match resolution {
            CancelReceiptResolution::ClosedWithoutFill => {
                inner.remove_effect(&track_id, effect_id);
                inner.mark_track_ready(&track_id);
                CancelQueueAction::UnblockedDownstream
            }
            CancelReceiptResolution::ClosedWithFill { .. } => {
                let effect_ids = inner.retire_current_batch_after(&track_id, effect_id);
                inner.mark_track_ready(&track_id);
                CancelQueueAction::SupersededDownstream {
                    effect_ids,
                    requires_reconcile: true,
                }
            }
            CancelReceiptResolution::StillWorking => {
                if let Some(track) = inner.tracks.get_mut(&track_id) {
                    track.paused_until = Some(DeferredUntil::ExchangeState);
                    if let Some(effect) = track.front_effect_mut() {
                        effect.dispatch_state = QueuedEffectState::Queued;
                    }
                }
                CancelQueueAction::Deferred {
                    until: DeferredUntil::ExchangeState,
                }
            }
            CancelReceiptResolution::Unknown { order_id, reason } => {
                let token = inner.next_follow_up_token(&track_id, effect_id, &order_id);
                if let Some((_track_id, effect)) = inner.effect_mut(effect_id) {
                    effect.dispatch_state = QueuedEffectState::AwaitingFollowUp;
                }
                CancelQueueAction::AwaitingFollowUpRetirement { reason, token }
            }
        }
    }

    pub fn resolve_follow_up_retirements_for_closed_orders(
        &self,
        track_id: &TrackId,
        open_order_ids: &HashSet<String>,
    ) -> Vec<FollowUpQueueAction> {
        let mut inner = self.inner.lock().unwrap();
        let tokens = inner
            .follow_up_tokens
            .iter()
            .filter_map(|(token, pointer)| {
                if &pointer.track_id == track_id
                    && !open_order_ids.contains(&pointer.closed_order_id)
                {
                    Some(token.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        tokens
            .into_iter()
            .map(|token| inner.record_follow_up_retirement_by_token(token))
            .collect()
    }

    pub fn record_follow_up_retirement(
        &self,
        resolution: FollowUpRetirementResolution,
    ) -> FollowUpQueueAction {
        let mut inner = self.inner.lock().unwrap();
        let Some(pointer) = inner.follow_up_tokens.get(&resolution.token) else {
            return FollowUpQueueAction::NothingToRetire;
        };
        if pointer.closed_order_id != resolution.closed_order_id {
            return FollowUpQueueAction::Blocked {
                reason: format!(
                    "follow-up token order mismatch: expected `{}`, got `{}`",
                    pointer.closed_order_id, resolution.closed_order_id
                ),
            };
        }
        inner.record_follow_up_retirement_by_token(resolution.token)
    }

    pub fn active_submit_effect_ids(&self) -> HashSet<String> {
        let inner = self.inner.lock().unwrap();
        inner
            .tracks
            .values()
            .flat_map(|track| track.batches.iter())
            .flat_map(|batch| batch.effects.iter())
            .filter(|item| {
                matches!(
                    item.dispatch_state,
                    QueuedEffectState::InFlight | QueuedEffectState::SubmittedAwaitingWriteback
                ) && matches!(item.effect.effect, TrackEffect::SubmitOrder { .. })
            })
            .map(|item| item.effect.effect_id.clone())
            .collect()
    }

    pub fn active_submit_hints_for_track(&self, track_id: &TrackId) -> Vec<PendingSubmitHint> {
        let inner = self.inner.lock().unwrap();
        let Some(track) = inner.tracks.get(track_id) else {
            return Vec::new();
        };
        track
            .batches
            .iter()
            .flat_map(|batch| batch.effects.iter())
            .filter(|item| {
                matches!(
                    item.dispatch_state,
                    QueuedEffectState::InFlight | QueuedEffectState::SubmittedAwaitingWriteback
                )
            })
            .filter_map(|item| match &item.effect.effect {
                TrackEffect::SubmitOrder {
                    request,
                    desired_exposure,
                    submit_purpose,
                    recovery_token,
                } => Some(PendingSubmitHint {
                    request: request.clone(),
                    desired_exposure: desired_exposure.clone(),
                    submit_purpose: *submit_purpose,
                    recovery_token: recovery_token.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    pub fn resolve_submitted_awaiting_exchange_state_for_track(
        &self,
        track_id: &TrackId,
    ) -> Vec<String> {
        let mut inner = self.inner.lock().unwrap();
        let mut resolved = Vec::new();
        {
            let Some(track) = inner.tracks.get_mut(track_id) else {
                return Vec::new();
            };

            for batch in &mut track.batches {
                let mut retained = VecDeque::new();
                while let Some(item) = batch.effects.pop_front() {
                    if item.dispatch_state == QueuedEffectState::SubmittedAwaitingWriteback
                        && matches!(item.effect.effect, TrackEffect::SubmitOrder { .. })
                    {
                        resolved.push(item.effect.effect_id.clone());
                    } else {
                        retained.push_back(item);
                    }
                }
                batch.effects = retained;
            }
            SessionEffectQueueInner::prune_empty_front_batches(track);
            track.paused_until = None;
        }
        for effect_id in &resolved {
            inner.effect_index.remove(effect_id.as_str());
        }
        inner.remove_empty_track(track_id);
        inner.mark_track_ready(track_id);
        resolved
    }

    pub fn snapshot_for_track(&self, track_id: &TrackId) -> SessionEffectQueueSnapshot {
        let inner = self.inner.lock().unwrap();
        let pending_effects = inner
            .tracks
            .get(track_id)
            .map(|track| {
                track
                    .batches
                    .iter()
                    .flat_map(|batch| batch.effects.iter())
                    .map(|item| {
                        let state = if let Some(until) = track.paused_until
                            && track
                                .batches
                                .front()
                                .is_some_and(|batch| batch.batch_id == item.effect.batch_id)
                            && track.front_effect().is_some_and(|effect| {
                                effect.effect.effect_id == item.effect.effect_id
                            }) {
                            SessionPendingEffectState::Deferred { until }
                        } else {
                            item.dispatch_state.into()
                        };
                        SessionPendingEffectView {
                            effect_id: item.effect.effect_id.clone(),
                            kind: item.effect.effect.kind(),
                            state,
                            created_at: item.effect.created_at,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        SessionEffectQueueSnapshot {
            track_id: track_id.clone(),
            pending_effects,
        }
    }

    pub fn clear_track(&self, track_id: &TrackId) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(track) = inner.tracks.remove(track_id) {
            for effect_id in track
                .batches
                .into_iter()
                .flat_map(|batch| batch.effects)
                .map(|effect| effect.effect.effect_id)
            {
                inner.effect_index.remove(effect_id.as_str());
            }
        }
        inner.ready_tracks.retain(|ready| ready != track_id);
    }
}

impl SessionEffectQueueInner {
    fn record_follow_up_retirement_by_token(
        &mut self,
        token: FollowUpRetirementToken,
    ) -> FollowUpQueueAction {
        let Some(pointer) = self.follow_up_tokens.remove(&token) else {
            return FollowUpQueueAction::NothingToRetire;
        };
        if !self
            .effect_index
            .contains_key(pointer.cancel_effect_id.as_str())
        {
            return FollowUpQueueAction::NothingToRetire;
        }

        let effect_ids =
            self.retire_current_batch_after(&pointer.track_id, &pointer.cancel_effect_id);
        self.mark_track_ready(&pointer.track_id);
        if effect_ids.is_empty() {
            FollowUpQueueAction::NothingToRetire
        } else {
            FollowUpQueueAction::SupersededDownstream {
                effect_ids,
                requires_reconcile: true,
            }
        }
    }
    fn mark_track_ready(&mut self, track_id: &TrackId) {
        let Some(track) = self.tracks.get_mut(track_id) else {
            return;
        };
        if track.paused_until.is_some() || track.in_ready_ring || !track.front_effect_is_queued() {
            return;
        }
        track.in_ready_ring = true;
        self.ready_tracks.push_back(track_id.clone());
    }

    fn remove_empty_track(&mut self, track_id: &TrackId) {
        if self
            .tracks
            .get(track_id)
            .is_some_and(|track| track.batches.is_empty())
        {
            self.tracks.remove(track_id);
            self.ready_tracks.retain(|ready| ready != track_id);
        }
    }

    fn effect_mut(&mut self, effect_id: &str) -> Option<(TrackId, &mut QueuedEffect)> {
        let track_id = self.effect_index.get(effect_id).cloned()?;
        let track = self.tracks.get_mut(&track_id)?;
        let effect = track
            .batches
            .iter_mut()
            .flat_map(|batch| batch.effects.iter_mut())
            .find(|item| item.effect.effect_id == effect_id)?;
        Some((track_id, effect))
    }

    fn remove_effect(&mut self, track_id: &TrackId, effect_id: &str) {
        let Some(track) = self.tracks.get_mut(track_id) else {
            return;
        };
        for batch in &mut track.batches {
            let before = batch.effects.len();
            batch
                .effects
                .retain(|item| item.effect.effect_id != effect_id);
            if batch.effects.len() != before {
                self.effect_index.remove(effect_id);
                break;
            }
        }
        Self::prune_empty_front_batches(track);
        self.remove_empty_track(track_id);
    }

    fn retire_current_batch_after(&mut self, track_id: &TrackId, effect_id: &str) -> Vec<String> {
        let Some(track) = self.tracks.get_mut(track_id) else {
            return Vec::new();
        };
        let Some(batch) = track.batches.front_mut() else {
            return Vec::new();
        };
        let mut retired = Vec::new();
        let mut remove_from_here = false;
        let mut retained = VecDeque::new();
        while let Some(item) = batch.effects.pop_front() {
            if item.effect.effect_id == effect_id {
                remove_from_here = true;
                self.effect_index.remove(item.effect.effect_id.as_str());
                continue;
            }
            if remove_from_here {
                retired.push(item.effect.effect_id.clone());
                self.effect_index.remove(item.effect.effect_id.as_str());
            } else {
                retained.push_back(item);
            }
        }
        batch.effects = retained;
        Self::prune_empty_front_batches(track);
        self.remove_empty_track(track_id);
        retired
    }

    fn prune_empty_front_batches(track: &mut TrackQueue) {
        while track
            .batches
            .front()
            .is_some_and(|batch| batch.effects.is_empty())
        {
            track.batches.pop_front();
        }
    }

    fn next_follow_up_token(
        &mut self,
        track_id: &TrackId,
        effect_id: &str,
        closed_order_id: &str,
    ) -> FollowUpRetirementToken {
        self.next_follow_up_token += 1;
        let token = FollowUpRetirementToken(format!(
            "{}:{}:{}",
            track_id.as_str(),
            effect_id,
            self.next_follow_up_token
        ));
        self.follow_up_tokens.insert(
            token.clone(),
            FollowUpPointer {
                track_id: track_id.clone(),
                cancel_effect_id: effect_id.to_string(),
                closed_order_id: closed_order_id.to_string(),
            },
        );
        token
    }
}

impl TrackQueue {
    fn front_effect(&self) -> Option<&QueuedEffect> {
        self.batches.front().and_then(|batch| batch.effects.front())
    }

    fn front_effect_mut(&mut self) -> Option<&mut QueuedEffect> {
        self.batches
            .front_mut()
            .and_then(|batch| batch.effects.front_mut())
    }

    fn front_effect_is_queued(&self) -> bool {
        self.front_effect()
            .is_some_and(|effect| effect.dispatch_state == QueuedEffectState::Queued)
    }
}

impl DeferredUntil {
    fn matches(self, signal: WakeSignal) -> bool {
        matches!(
            (self, signal),
            (Self::FreshMarket, WakeSignal::FreshMarket)
                | (Self::ExchangeState, WakeSignal::ExchangeState)
        )
    }
}

impl From<QueuedEffectState> for SessionPendingEffectState {
    fn from(value: QueuedEffectState) -> Self {
        match value {
            QueuedEffectState::Queued => Self::Queued,
            QueuedEffectState::InFlight => Self::InFlight,
            QueuedEffectState::SubmittedAwaitingWriteback => Self::SubmittedAwaitingWriteback,
            QueuedEffectState::AwaitingFollowUp => Self::AwaitingFollowUp,
        }
    }
}

trait SessionTrackEffectExt {
    fn kind(&self) -> SessionPendingEffectKind;
}

impl SessionTrackEffectExt for TrackEffect {
    fn kind(&self) -> SessionPendingEffectKind {
        match self {
            TrackEffect::SubmitOrder { .. } => SessionPendingEffectKind::Submit,
            TrackEffect::CancelOrder { .. } | TrackEffect::CancelAll { .. } => {
                SessionPendingEffectKind::Cancel
            }
            TrackEffect::NoOp => SessionPendingEffectKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use poise_core::types::{Exposure, Side};
    use poise_engine::executor::SubmitRecoveryToken;
    use poise_engine::ports::OrderRequest;
    use poise_engine::price_gate::SubmitPurpose;
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;

    use super::{
        CancelQueueAction, CancelReceiptResolution, DeferredUntil, FollowUpQueueAction,
        FollowUpRetirementResolution, SessionEffectOutcome, SessionEffectQueue,
        SessionPendingEffectState, SessionQueueAction, SessionTrackEffect, WakeSignal,
    };

    fn cancel_effect(effect_id: &str, batch_id: &str, sequence: u32) -> SessionTrackEffect {
        SessionTrackEffect {
            effect_id: effect_id.to_string(),
            track_id: TrackId::new("btc-core"),
            batch_id: batch_id.to_string(),
            sequence,
            effect: TrackEffect::CancelOrder {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                order_id: "order-1".into(),
            },
            created_at: Utc::now(),
        }
    }

    fn effect(effect_id: &str, batch_id: &str, sequence: u32) -> SessionTrackEffect {
        effect_for_track("btc-core", effect_id, batch_id, sequence)
    }

    fn effect_for_track(
        track_id: &str,
        effect_id: &str,
        batch_id: &str,
        sequence: u32,
    ) -> SessionTrackEffect {
        SessionTrackEffect {
            effect_id: effect_id.to_string(),
            track_id: TrackId::new(track_id),
            batch_id: batch_id.to_string(),
            sequence,
            effect: TrackEffect::CancelAll {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            },
            created_at: Utc::now(),
        }
    }

    fn submit_effect(effect_id: &str, batch_id: &str, sequence: u32) -> SessionTrackEffect {
        SessionTrackEffect {
            effect_id: effect_id.to_string(),
            track_id: TrackId::new("btc-core"),
            batch_id: batch_id.to_string(),
            sequence,
            effect: TrackEffect::SubmitOrder {
                request: OrderRequest {
                    instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                    side: Side::Buy,
                    price: 100.0,
                    quantity: 0.1,
                    client_order_id: "client-1".into(),
                    reduce_only: false,
                },
                desired_exposure: Exposure(4.0),
                submit_purpose: SubmitPurpose::AutoReconcile,
                recovery_token: SubmitRecoveryToken::empty(),
            },
            created_at: Utc::now(),
        }
    }

    #[test]
    fn queue_dispatches_batch_in_sequence_order() {
        let queue = SessionEffectQueue::default();
        queue.enqueue_batch(vec![
            effect("effect-2", "batch-1", 2),
            effect("effect-1", "batch-1", 1),
        ]);

        assert_eq!(queue.claim_next().unwrap().effect_id, "effect-1");
        queue.record_outcome("effect-1", SessionEffectOutcome::Finished);
        assert_eq!(queue.claim_next().unwrap().effect_id, "effect-2");
    }

    #[test]
    fn blocked_effect_retires_current_batch_and_allows_next_batch() {
        let queue = SessionEffectQueue::default();
        queue.enqueue_batch(vec![
            effect("effect-1", "batch-1", 1),
            effect("effect-2", "batch-1", 2),
        ]);
        queue.enqueue_batch(vec![effect("effect-3", "batch-2", 1)]);

        assert_eq!(queue.claim_next().unwrap().effect_id, "effect-1");
        let action = queue.record_outcome(
            "effect-1",
            SessionEffectOutcome::Blocked {
                reason: "cancel failed".to_string(),
            },
        );
        assert_eq!(
            action,
            SessionQueueAction::RetiredBatch {
                effect_ids: vec!["effect-2".into()],
                requires_reconcile: true,
            }
        );
        assert_eq!(queue.claim_next().unwrap().effect_id, "effect-3");
    }

    #[test]
    fn deferred_effect_blocks_only_its_track_until_matching_wake() {
        let queue = SessionEffectQueue::default();
        queue.enqueue_batch(vec![effect_for_track(
            "btc-core",
            "btc-effect-1",
            "btc-batch",
            1,
        )]);
        queue.enqueue_batch(vec![effect_for_track(
            "eth-core",
            "eth-effect-1",
            "eth-batch",
            1,
        )]);

        assert_eq!(queue.claim_next().unwrap().effect_id, "btc-effect-1");
        queue.record_outcome(
            "btc-effect-1",
            SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            },
        );
        assert_eq!(queue.claim_next().unwrap().effect_id, "eth-effect-1");
        queue.record_outcome("eth-effect-1", SessionEffectOutcome::Finished);
        assert!(queue.claim_next().is_none());

        queue.wake_track_for(&TrackId::new("btc-core"), WakeSignal::FreshMarket);
        assert!(
            queue.claim_next().is_none(),
            "market wake must not wake an exchange-state deferred effect"
        );

        queue.wake_track_for(&TrackId::new("btc-core"), WakeSignal::ExchangeState);
        assert_eq!(queue.claim_next().unwrap().effect_id, "btc-effect-1");
    }

    #[test]
    fn queue_snapshot_exposes_display_dto_without_batch_ordering() {
        let queue = SessionEffectQueue::default();
        queue.enqueue_batch(vec![effect("effect-1", "batch-1", 0)]);

        let snapshot = queue.snapshot_for_track(&TrackId::new("btc-core"));

        assert_eq!(snapshot.pending_effects.len(), 1);
        assert_eq!(snapshot.pending_effects[0].effect_id, "effect-1");
        assert_eq!(
            snapshot.pending_effects[0].state,
            SessionPendingEffectState::Queued
        );
    }

    #[test]
    fn submit_exchange_accepted_records_writeback_window() {
        let queue = SessionEffectQueue::default();
        queue.enqueue_batch(vec![submit_effect("submit-1", "batch-1", 0)]);

        assert_eq!(queue.claim_next().unwrap().effect_id, "submit-1");
        assert!(
            queue.record_submit_exchange_accepted("submit-1"),
            "queue should own submit dispatch progress"
        );

        let snapshot = queue.snapshot_for_track(&TrackId::new("btc-core"));
        assert_eq!(
            snapshot.pending_effects[0].state,
            SessionPendingEffectState::SubmittedAwaitingWriteback
        );
        assert_eq!(
            queue
                .active_submit_hints_for_track(&TrackId::new("btc-core"))
                .len(),
            1,
            "accepted submit remains visible to exchange sync until writeback finishes"
        );
    }

    #[test]
    fn submitted_writeback_unknown_keeps_active_hint_until_exchange_sync() {
        let queue = SessionEffectQueue::default();
        let track_id = TrackId::new("btc-core");
        queue.enqueue_batch(vec![submit_effect("submit-1", "batch-1", 0)]);

        assert_eq!(queue.claim_next().unwrap().effect_id, "submit-1");
        assert!(queue.record_submit_exchange_accepted("submit-1"));
        queue.record_outcome(
            "submit-1",
            SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            },
        );

        assert_eq!(
            queue.active_submit_hints_for_track(&track_id).len(),
            1,
            "accepted submit must remain visible to exchange sync while writeback is unknown"
        );
        assert!(
            queue.claim_next().is_none(),
            "writeback-unknown submit should not be redispatched while waiting for exchange state"
        );

        assert_eq!(
            queue.resolve_submitted_awaiting_exchange_state_for_track(&track_id),
            vec!["submit-1".to_string()]
        );
        assert!(
            queue.active_submit_hints_for_track(&track_id).is_empty(),
            "complete exchange sync should retire the active submit hint"
        );
    }

    #[test]
    fn active_submit_hints_exclude_future_queued_submit() {
        let queue = SessionEffectQueue::default();
        queue.enqueue_batch(vec![submit_effect("submit-1", "batch-1", 0)]);

        assert!(
            queue
                .active_submit_hints_for_track(&TrackId::new("btc-core"))
                .is_empty(),
            "queued submit that was never claimed is not an exchange fact"
        );

        assert_eq!(queue.claim_next().unwrap().effect_id, "submit-1");
        assert_eq!(
            queue
                .active_submit_hints_for_track(&TrackId::new("btc-core"))
                .len(),
            1
        );
    }

    #[test]
    fn cancel_without_fill_unblocks_downstream_submit_effects() {
        let queue = SessionEffectQueue::default();
        queue.enqueue_batch(vec![
            cancel_effect("cancel-1", "batch-1", 0),
            submit_effect("submit-1", "batch-1", 1),
            submit_effect("submit-2", "batch-1", 2),
        ]);

        assert_eq!(queue.claim_next().unwrap().effect_id, "cancel-1");

        let action =
            queue.record_cancel_resolution("cancel-1", CancelReceiptResolution::ClosedWithoutFill);

        assert_eq!(action, CancelQueueAction::UnblockedDownstream);
        assert_eq!(queue.claim_next().unwrap().effect_id, "submit-1");
    }

    #[test]
    fn cancel_with_fill_supersedes_downstream_submit_effects() {
        let queue = SessionEffectQueue::default();
        queue.enqueue_batch(vec![
            cancel_effect("cancel-1", "batch-1", 0),
            submit_effect("submit-1", "batch-1", 1),
            submit_effect("submit-2", "batch-1", 2),
        ]);

        assert_eq!(queue.claim_next().unwrap().effect_id, "cancel-1");

        let action = queue.record_cancel_resolution(
            "cancel-1",
            CancelReceiptResolution::ClosedWithFill { filled_qty: 0.4 },
        );

        assert_eq!(
            action,
            CancelQueueAction::SupersededDownstream {
                effect_ids: vec!["submit-1".into(), "submit-2".into()],
                requires_reconcile: true,
            }
        );
        assert!(queue.claim_next().is_none());
    }

    #[test]
    fn follow_up_retirement_token_is_resolved_by_queue() {
        let queue = SessionEffectQueue::default();
        queue.enqueue_batch(vec![
            cancel_effect("cancel-1", "batch-1", 0),
            submit_effect("submit-1", "batch-1", 1),
            submit_effect("submit-2", "batch-1", 2),
        ]);

        assert_eq!(queue.claim_next().unwrap().effect_id, "cancel-1");
        let action = queue.record_cancel_resolution(
            "cancel-1",
            CancelReceiptResolution::Unknown {
                order_id: "closed-order".into(),
                reason: "exchange returned unknown order".into(),
            },
        );
        let token = match action {
            CancelQueueAction::AwaitingFollowUpRetirement { token, .. } => token,
            other => panic!("expected follow-up token, got {other:?}"),
        };

        let action = queue.record_follow_up_retirement(FollowUpRetirementResolution {
            token,
            closed_order_id: "closed-order".into(),
        });

        assert_eq!(
            action,
            FollowUpQueueAction::SupersededDownstream {
                effect_ids: vec!["submit-1".into(), "submit-2".into()],
                requires_reconcile: true,
            }
        );
    }
}
