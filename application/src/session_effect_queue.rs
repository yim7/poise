use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use poise_engine::executor::PendingSubmitHint;
use poise_engine::observation::CompleteOpenOrderSnapshot;
use poise_engine::track::TrackId;
use poise_engine::transition::TrackEffect;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionTrackEffect {
    pub effect_id: String,
    pub track_id: TrackId,
    pub effect: TrackEffect,
    pub created_at: DateTime<Utc>,
    pub(crate) batch_id: String,
    pub(crate) sequence: u32,
}

impl SessionTrackEffect {
    fn prepare_transition_effects(
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
    AwaitingCancelFollowUp {
        reason: String,
    },
    Blocked {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct InternalFollowUpKey(String);

#[derive(Debug, Clone, PartialEq)]
pub enum FollowUpQueueAction {
    Closed {
        cancel_effect_id: String,
        superseded_downstream_effect_ids: Vec<String>,
        requires_reconcile: bool,
    },
    StillOpen {
        order_id: String,
    },
    NoMatchingFollowUp,
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
    follow_up_tokens: HashMap<InternalFollowUpKey, FollowUpPointer>,
    next_follow_up_token: u64,
    next_batch_id: u64,
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
    pub fn enqueue_transition_effects(
        &self,
        track_id: &TrackId,
        effects: &[TrackEffect],
        created_at: DateTime<Utc>,
    ) -> Vec<SessionTrackEffect> {
        let batch_id = {
            let mut inner = self.inner.lock().unwrap();
            inner.next_batch_id_for(track_id)
        };
        let session_effects = SessionTrackEffect::prepare_transition_effects(
            track_id, &batch_id, effects, created_at,
        );
        self.enqueue_prepared_effects(session_effects.clone());
        session_effects
    }

    fn enqueue_prepared_effects(&self, effects: Vec<SessionTrackEffect>) {
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
                inner.next_follow_up_token(&track_id, effect_id, &order_id);
                if let Some((_track_id, effect)) = inner.effect_mut(effect_id) {
                    effect.dispatch_state = QueuedEffectState::AwaitingFollowUp;
                }
                CancelQueueAction::AwaitingCancelFollowUp { reason }
            }
        }
    }

    pub fn resolve_cancel_follow_ups_from_open_order_snapshot(
        &self,
        track_id: &TrackId,
        open_orders: &CompleteOpenOrderSnapshot,
    ) -> Vec<FollowUpQueueAction> {
        let mut inner = self.inner.lock().unwrap();
        let open_order_ids = open_orders
            .orders()
            .iter()
            .map(|order| order.order_id.as_str())
            .collect::<HashSet<_>>();
        let tokens = inner
            .follow_up_tokens
            .iter()
            .filter_map(|(token, pointer)| {
                if &pointer.track_id == track_id {
                    Some((
                        token.clone(),
                        open_order_ids.contains(pointer.closed_order_id.as_str()),
                    ))
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        tokens
            .into_iter()
            .map(|(token, still_open)| {
                if still_open {
                    inner.record_follow_up_still_working_by_token(token)
                } else {
                    inner.record_follow_up_closed_by_token(token)
                }
            })
            .collect()
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
            if resolved.is_empty() {
                return resolved;
            }
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
    fn record_follow_up_still_working_by_token(
        &mut self,
        token: InternalFollowUpKey,
    ) -> FollowUpQueueAction {
        let Some(pointer) = self.follow_up_tokens.remove(&token) else {
            return FollowUpQueueAction::NoMatchingFollowUp;
        };
        let order_id = pointer.closed_order_id.clone();
        let Some(track) = self.tracks.get_mut(&pointer.track_id) else {
            return FollowUpQueueAction::NoMatchingFollowUp;
        };
        let Some(effect) = track.front_effect_mut() else {
            return FollowUpQueueAction::NoMatchingFollowUp;
        };
        if effect.effect.effect_id != pointer.cancel_effect_id {
            return FollowUpQueueAction::Blocked {
                reason: format!(
                    "follow-up cancel effect `{}` is no longer at the front of track `{}`",
                    pointer.cancel_effect_id,
                    pointer.track_id.as_str()
                ),
            };
        }
        effect.dispatch_state = QueuedEffectState::Queued;
        track.paused_until = Some(DeferredUntil::ExchangeState);
        FollowUpQueueAction::StillOpen { order_id }
    }

    fn record_follow_up_closed_by_token(
        &mut self,
        token: InternalFollowUpKey,
    ) -> FollowUpQueueAction {
        let Some(pointer) = self.follow_up_tokens.remove(&token) else {
            return FollowUpQueueAction::NoMatchingFollowUp;
        };
        if !self
            .effect_index
            .contains_key(pointer.cancel_effect_id.as_str())
        {
            return FollowUpQueueAction::NoMatchingFollowUp;
        }

        let effect_ids =
            self.retire_current_batch_after(&pointer.track_id, &pointer.cancel_effect_id);
        self.mark_track_ready(&pointer.track_id);
        FollowUpQueueAction::Closed {
            cancel_effect_id: pointer.cancel_effect_id,
            superseded_downstream_effect_ids: effect_ids,
            requires_reconcile: true,
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
    ) -> InternalFollowUpKey {
        self.next_follow_up_token += 1;
        let token = InternalFollowUpKey(format!(
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

    fn next_batch_id_for(&mut self, track_id: &TrackId) -> String {
        self.next_batch_id += 1;
        format!("{}:batch:{}", track_id.as_str(), self.next_batch_id)
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
    use poise_engine::observation::{CompleteOpenOrderSnapshot, OrderObservation};
    use poise_engine::ports::{OrderRequest, OrderStatus};
    use poise_engine::price_gate::SubmitPurpose;
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_engine::transition::TrackEffect;

    use super::{
        CancelQueueAction, CancelReceiptResolution, DeferredUntil, FollowUpQueueAction,
        SessionEffectOutcome, SessionEffectQueue, SessionPendingEffectState, SessionQueueAction,
        WakeSignal,
    };

    fn enqueue_effects(
        queue: &SessionEffectQueue,
        track_id: &str,
        effects: &[TrackEffect],
    ) -> Vec<String> {
        queue
            .enqueue_transition_effects(&TrackId::new(track_id), effects, Utc::now())
            .into_iter()
            .map(|effect| effect.effect_id)
            .collect()
    }

    fn cancel_effect() -> TrackEffect {
        TrackEffect::CancelOrder {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
            order_id: "order-1".into(),
        }
    }

    fn cancel_all_effect() -> TrackEffect {
        TrackEffect::CancelAll {
            instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
        }
    }

    fn submit_effect(client_order_id: &str) -> TrackEffect {
        TrackEffect::SubmitOrder {
            request: OrderRequest {
                instrument: Instrument::new(Venue::Binance, "BTCUSDT"),
                side: Side::Buy,
                price: 100.0,
                quantity: 0.1,
                client_order_id: client_order_id.into(),
                reduce_only: false,
            },
            desired_exposure: Exposure(4.0),
            submit_purpose: SubmitPurpose::AutoReconcile,
            recovery_token: SubmitRecoveryToken::empty(),
        }
    }

    fn complete_open_orders(order_ids: &[&str]) -> CompleteOpenOrderSnapshot {
        CompleteOpenOrderSnapshot::from_complete_exchange_query(
            order_ids
                .iter()
                .map(|order_id| OrderObservation {
                    order_id: (*order_id).to_string(),
                    client_order_id: format!("{order_id}-client"),
                    side: Side::Buy,
                    price: 100.0,
                    quantity: 0.1,
                    filled_qty: 0.0,
                    realized_pnl: 0.0,
                    status: OrderStatus::New,
                })
                .collect(),
        )
    }

    #[test]
    fn enqueue_transition_effects_generates_batch_identity_inside_queue() {
        let queue = SessionEffectQueue::default();
        let track_id = TrackId::new("btc-core");
        let enqueued = queue.enqueue_transition_effects(
            &track_id,
            &[cancel_all_effect(), cancel_all_effect()],
            Utc::now(),
        );

        assert_eq!(enqueued.len(), 2);
        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0].effect_id);
        queue.record_outcome(&enqueued[0].effect_id, SessionEffectOutcome::Finished);
        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[1].effect_id);
    }

    #[test]
    fn queue_dispatches_batch_in_sequence_order() {
        let queue = SessionEffectQueue::default();
        let enqueued = enqueue_effects(
            &queue,
            "btc-core",
            &[cancel_all_effect(), cancel_all_effect()],
        );

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);
        queue.record_outcome(&enqueued[0], SessionEffectOutcome::Finished);
        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[1]);
    }

    #[test]
    fn blocked_effect_retires_current_batch_and_allows_next_batch() {
        let queue = SessionEffectQueue::default();
        let first_batch = enqueue_effects(
            &queue,
            "btc-core",
            &[cancel_all_effect(), cancel_all_effect()],
        );
        let second_batch = enqueue_effects(&queue, "btc-core", &[cancel_all_effect()]);

        assert_eq!(queue.claim_next().unwrap().effect_id, first_batch[0]);
        let action = queue.record_outcome(
            &first_batch[0],
            SessionEffectOutcome::Blocked {
                reason: "cancel failed".to_string(),
            },
        );
        assert_eq!(
            action,
            SessionQueueAction::RetiredBatch {
                effect_ids: vec![first_batch[1].clone()],
                requires_reconcile: true,
            }
        );
        assert_eq!(queue.claim_next().unwrap().effect_id, second_batch[0]);
    }

    #[test]
    fn deferred_effect_blocks_only_its_track_until_matching_wake() {
        let queue = SessionEffectQueue::default();
        let btc_effect = enqueue_effects(&queue, "btc-core", &[cancel_all_effect()]);
        let eth_effect = enqueue_effects(&queue, "eth-core", &[cancel_all_effect()]);

        assert_eq!(queue.claim_next().unwrap().effect_id, btc_effect[0]);
        queue.record_outcome(
            &btc_effect[0],
            SessionEffectOutcome::Deferred {
                until: DeferredUntil::ExchangeState,
            },
        );
        assert_eq!(queue.claim_next().unwrap().effect_id, eth_effect[0]);
        queue.record_outcome(&eth_effect[0], SessionEffectOutcome::Finished);
        assert!(queue.claim_next().is_none());

        queue.wake_track_for(&TrackId::new("btc-core"), WakeSignal::FreshMarket);
        assert!(
            queue.claim_next().is_none(),
            "market wake must not wake an exchange-state deferred effect"
        );

        queue.wake_track_for(&TrackId::new("btc-core"), WakeSignal::ExchangeState);
        assert_eq!(queue.claim_next().unwrap().effect_id, btc_effect[0]);
    }

    #[test]
    fn queue_snapshot_exposes_display_dto_without_batch_ordering() {
        let queue = SessionEffectQueue::default();
        let enqueued = enqueue_effects(&queue, "btc-core", &[cancel_all_effect()]);

        let snapshot = queue.snapshot_for_track(&TrackId::new("btc-core"));

        assert_eq!(snapshot.pending_effects.len(), 1);
        assert_eq!(snapshot.pending_effects[0].effect_id, enqueued[0]);
        assert_eq!(
            snapshot.pending_effects[0].state,
            SessionPendingEffectState::Queued
        );
    }

    #[test]
    fn submit_exchange_accepted_records_writeback_window() {
        let queue = SessionEffectQueue::default();
        let enqueued = enqueue_effects(&queue, "btc-core", &[submit_effect("client-1")]);

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);
        assert!(
            queue.record_submit_exchange_accepted(&enqueued[0]),
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
        let enqueued = enqueue_effects(&queue, "btc-core", &[submit_effect("client-1")]);

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);
        assert!(queue.record_submit_exchange_accepted(&enqueued[0]));
        queue.record_outcome(
            &enqueued[0],
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
            vec![enqueued[0].clone()]
        );
        assert!(
            queue.active_submit_hints_for_track(&track_id).is_empty(),
            "complete exchange sync should retire the active submit hint"
        );
    }

    #[test]
    fn active_submit_hints_exclude_future_queued_submit() {
        let queue = SessionEffectQueue::default();
        let enqueued = enqueue_effects(&queue, "btc-core", &[submit_effect("client-1")]);

        assert!(
            queue
                .active_submit_hints_for_track(&TrackId::new("btc-core"))
                .is_empty(),
            "queued submit that was never claimed is not an exchange fact"
        );

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);
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
        let enqueued = enqueue_effects(
            &queue,
            "btc-core",
            &[
                cancel_effect(),
                submit_effect("client-1"),
                submit_effect("client-2"),
            ],
        );

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);

        let action = queue
            .record_cancel_resolution(&enqueued[0], CancelReceiptResolution::ClosedWithoutFill);

        assert_eq!(action, CancelQueueAction::UnblockedDownstream);
        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[1]);
    }

    #[test]
    fn cancel_with_fill_supersedes_downstream_submit_effects() {
        let queue = SessionEffectQueue::default();
        let enqueued = enqueue_effects(
            &queue,
            "btc-core",
            &[
                cancel_effect(),
                submit_effect("client-1"),
                submit_effect("client-2"),
            ],
        );

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);

        let action = queue.record_cancel_resolution(
            &enqueued[0],
            CancelReceiptResolution::ClosedWithFill { filled_qty: 0.4 },
        );

        assert_eq!(
            action,
            CancelQueueAction::SupersededDownstream {
                effect_ids: vec![enqueued[1].clone(), enqueued[2].clone()],
                requires_reconcile: true,
            }
        );
        assert!(queue.claim_next().is_none());
    }

    #[test]
    fn cancel_follow_up_is_resolved_from_complete_open_order_snapshot() {
        let queue = SessionEffectQueue::default();
        let enqueued = enqueue_effects(
            &queue,
            "btc-core",
            &[
                cancel_effect(),
                submit_effect("client-1"),
                submit_effect("client-2"),
            ],
        );

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);
        let action = queue.record_cancel_resolution(
            &enqueued[0],
            CancelReceiptResolution::Unknown {
                order_id: "closed-order".into(),
                reason: "exchange returned unknown order".into(),
            },
        );
        assert_eq!(
            action,
            CancelQueueAction::AwaitingCancelFollowUp {
                reason: "exchange returned unknown order".into(),
            }
        );

        let actions = queue.resolve_cancel_follow_ups_from_open_order_snapshot(
            &TrackId::new("btc-core"),
            &complete_open_orders(&[]),
        );

        assert_eq!(
            actions,
            vec![FollowUpQueueAction::Closed {
                cancel_effect_id: enqueued[0].clone(),
                superseded_downstream_effect_ids: vec![enqueued[1].clone(), enqueued[2].clone()],
                requires_reconcile: true,
            }]
        );
    }

    #[test]
    fn closed_cancel_follow_up_requires_reconcile_even_without_downstream_submit() {
        let queue = SessionEffectQueue::default();
        let track_id = TrackId::new("btc-core");
        let enqueued = enqueue_effects(&queue, track_id.as_str(), &[cancel_effect()]);

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);
        queue.record_cancel_resolution(
            &enqueued[0],
            CancelReceiptResolution::Unknown {
                order_id: "closed-order".into(),
                reason: "cancel outcome unknown".into(),
            },
        );

        let actions = queue.resolve_cancel_follow_ups_from_open_order_snapshot(
            &track_id,
            &complete_open_orders(&[]),
        );

        assert_eq!(
            actions,
            vec![FollowUpQueueAction::Closed {
                cancel_effect_id: enqueued[0].clone(),
                superseded_downstream_effect_ids: vec![],
                requires_reconcile: true,
            }]
        );
    }

    #[test]
    fn follow_up_snapshot_with_still_open_order_retries_cancel_after_exchange_wake() {
        let queue = SessionEffectQueue::default();
        let track_id = TrackId::new("btc-core");
        let enqueued = enqueue_effects(
            &queue,
            track_id.as_str(),
            &[cancel_effect(), submit_effect("client-1")],
        );

        assert_eq!(queue.claim_next().unwrap().effect_id, enqueued[0]);
        queue.record_cancel_resolution(
            &enqueued[0],
            CancelReceiptResolution::Unknown {
                order_id: "order-1".into(),
                reason: "cancel outcome unknown".into(),
            },
        );

        let actions = queue.resolve_cancel_follow_ups_from_open_order_snapshot(
            &track_id,
            &complete_open_orders(&["order-1"]),
        );

        assert_eq!(
            actions,
            vec![FollowUpQueueAction::StillOpen {
                order_id: "order-1".into()
            }]
        );
        assert!(
            queue.claim_next().is_none(),
            "still-open order should not immediately release downstream submit"
        );
        queue.wake_track_for(&track_id, WakeSignal::ExchangeState);
        assert_eq!(
            queue.claim_next().unwrap().effect_id,
            enqueued[0],
            "the original cancel should be retried before downstream submit"
        );
        assert!(queue.active_submit_hints_for_track(&track_id).is_empty());
    }
}
