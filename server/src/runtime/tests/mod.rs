use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::{TimeZone, Utc};
use poise_application::{
    AccountMonitor, AccountMonitorConfig, AccountMonitorStore, CommittedTrackWrite, EffectStatus,
    EffectStatusUpdate, FollowUpRetirementRequest, PersistedTrackEffect, StoredAccountMonitorState,
    StoredTrackEvent, StoredTrackSnapshot, TrackEffectStore, TrackMutationError,
    TrackMutationStore, TrackQueryService, TrackQueryStore,
};
use poise_core::events::DomainEvent;
use poise_core::risk::CapacityBudget;
use poise_core::strategy::{OutOfBandPolicy, ShapeFamily, TrackConfig};
use poise_core::types::{ExchangeRules, Exposure, Side};
use poise_engine::command::TrackCommand;
use poise_engine::execution_plan::ExecutionAction;
use poise_engine::executor::{ExecutionMode, OrderRole, OrderSlot};
use poise_engine::ledger::{
    ExecutionLedgerUpdate, LedgerAdjustmentEvent, LedgerDelta, TrackLedgerEvent,
};
use poise_engine::manager::{ExchangeSyncMode, TrackManager};
use poise_engine::observation::OrderObservation;
use poise_engine::ports::{
    AccountCapacitySnapshot, AccountPort, AccountSummaryPort, ClockPort, ExchangeInfo,
    ExchangeOrder, ExecutionPort, MarketDataPort, MetadataPort, OrderReceipt, OrderRequest,
    OrderStatus, Position, PriceTick, TrackLedgerUpdate, UserDataEvent, UserDataPayload,
};
use poise_engine::runtime::{
    ExecutionSlot, ExecutionStats, ExecutorState, RiskState, SlotState, TrackStatus, WorkingOrder,
};
use poise_engine::snapshot::TrackRuntimeSnapshot;
use poise_engine::track::{Instrument, TrackId, Venue};
use poise_engine::transition::TrackEffect;
use tokio::sync::{Mutex as AsyncMutex, Notify, broadcast, mpsc};
use tokio::time::{sleep, timeout};

use crate::effect_worker::EffectWorker;
use crate::exchange_freshness::ExchangeFreshnessReason;
use crate::projector::TrackProjector;
use crate::test_support::{
    EffectWorkerTestContext, RuntimeTestContext, build_effect_worker_test_context,
    build_runtime_and_effect_worker_test_contexts, build_runtime_test_context,
    build_test_application_services,
};

use super::{
    AccountMarginGuardStore, RuntimeHandles, RuntimePorts, RuntimeStartupCapacityMode,
    RuntimeStartupDefinition, ServerRuntime, enqueue_reconcile_request,
    exchange_state::{apply_user_data_event, order_observation, position_observation},
    sync_exchange_state_from_exchange,
};

mod execution;
mod reconcile;
mod startup;
mod support;
mod user_data;

use support::*;
