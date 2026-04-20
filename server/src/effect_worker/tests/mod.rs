use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{TimeZone, Utc};
use poise_application::{
    CommittedTrackWrite, EffectStatus, EffectStatusUpdate, FollowUpRetirementRequest,
    PersistedTrackEffect, StoredTrackEvent, StoredTrackSnapshot, TrackEffectStore,
    TrackMutationStore, TrackQueryStore,
};
use poise_core::risk::CapacityBudget;
use poise_core::strategy::{BandProtectionPolicy, BandRecoverPolicy, ShapeFamily, TrackConfig};
use poise_core::types::{ExchangeRules, Exposure, Side};
use poise_engine::executor::{ExecutionMode, ExecutionReason, RecoveryAnomaly};
use poise_engine::manager::TrackManager;
use poise_engine::observation::OrderObservation;
use poise_engine::ports::{
    AccountPort, ClockPort, ExchangeOrder, ExecutionPort, OrderReceipt, OrderRequest, OrderStatus,
    Position,
};
use poise_engine::price_gate::SubmitPurpose;
use poise_engine::runtime::{ExecutionStats, ExecutorState, RiskState, SlotState, WorkingOrder};
use poise_engine::snapshot::{ObservedState, TrackRuntimeSnapshot};
use poise_engine::track::{Instrument, TrackId, Venue};
use poise_engine::transition::TrackEffect;
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, watch};
use tokio::time::timeout;

use crate::exchange_freshness::ExchangeFreshnessReason;
use crate::submit_preflight::{SubmitPreflight, SubmitPreflightDecision};
use crate::test_support::{
    EffectWorkerTestContext, build_effect_worker_test_context, build_test_application_services,
};

use super::{Cancellation, EffectWorker};

mod dispatch;
mod execute;
mod retry;
mod support;

use support::*;
