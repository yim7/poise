use anyhow::Result;
use async_trait::async_trait;
use poise_core::events::DomainEvent;
use poise_core::track::TrackId;
use poise_engine::ledger::TrackLedgerState;

use crate::TrackControlState;
use crate::track_persistence::CommittedTrackWrite;

#[async_trait]
pub trait TrackMutationStore: Send + Sync {
    /// 提交会改变业务真值的 track transition。
    /// store owner 必须在同一个原子提交里确保 durable truth 完整；
    /// 如果持久控制真值尚不存在而调用方也没有显式提供 `control_state`，
    /// 实现需要补齐默认的 `TrackControlState::Enabled { Automatic }`。
    async fn commit_track_transition(
        &self,
        id: &str,
        control_state: Option<&TrackControlState>,
        ledger_state: &TrackLedgerState,
        events: &[DomainEvent],
    ) -> Result<CommittedTrackWrite>;

    async fn list_track_events(&self, id: &str) -> Result<Vec<DomainEvent>>;
    async fn save_track_control_state(
        &self,
        track_id: &TrackId,
        state: &TrackControlState,
    ) -> Result<()>;
    async fn save_track_ledger_state(
        &self,
        track_id: &TrackId,
        state: &TrackLedgerState,
    ) -> Result<()>;
}
