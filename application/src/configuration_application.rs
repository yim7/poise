use async_trait::async_trait;
use poise_core::track::{TrackDefinition, TrackId};

/// Durable request identity for a configuration application.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigurationApplicationRequest {
    pub application_id: String,
    pub request_hash: String,
    pub expected_version: String,
    pub candidate_version: String,
    pub candidate: TrackDefinition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigurationApplicationStatus {
    Idle,
    Preparing,
    WaitingForOrders,
    Activating,
    Active,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigurationApplicationPersistence {
    Persisted,
    AlreadyPersisted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigurationApplicationStoreError {
    VersionConflict,
    ApplicationIdConflict,
    ExternalChange,
    WriteFailed(String),
    Internal(String),
}

impl std::fmt::Display for ConfigurationApplicationStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for ConfigurationApplicationStoreError {}

/// Server owns the durable configuration file and idempotency record.
#[async_trait]
pub trait ConfigurationApplicationStore: Send + Sync {
    async fn persist_configuration_application(
        &self,
        track_id: &TrackId,
        request: &ConfigurationApplicationRequest,
    ) -> std::result::Result<ConfigurationApplicationPersistence, ConfigurationApplicationStoreError>;

    async fn configuration_application_status(
        &self,
        track_id: &TrackId,
    ) -> std::result::Result<ConfigurationApplicationStatus, ConfigurationApplicationStoreError>;

    /// Records the terminal result after the runtime has either installed the
    /// already-persisted definition or rejected the installation.
    async fn complete_configuration_application(
        &self,
        track_id: &TrackId,
        request: &ConfigurationApplicationRequest,
        status: ConfigurationApplicationStatus,
    ) -> std::result::Result<(), ConfigurationApplicationStoreError>;
}
