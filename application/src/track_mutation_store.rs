use anyhow::Result;
use async_trait::async_trait;
use poise_core::events::DomainEvent;
use poise_core::instrument::InstrumentSpec;
use poise_core::track::TrackId;
use poise_engine::ledger::TrackPnlRecord;

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
        events: &[DomainEvent],
    ) -> Result<CommittedTrackWrite>;

    async fn list_track_events(&self, id: &str) -> Result<Vec<DomainEvent>>;
    async fn save_track_control_state(
        &self,
        track_id: &TrackId,
        state: &TrackControlState,
    ) -> Result<()>;
    /// 保存交易所 cashflow 事实；`instrument_spec` 是记录入库时的合约规格，
    /// store 必须与成交数量一起保存其 canonical unit 与合约面值语义。
    async fn insert_track_pnl_record(
        &self,
        track_id: &TrackId,
        record: &TrackPnlRecord,
        instrument_spec: &InstrumentSpec,
    ) -> Result<bool>;
}
