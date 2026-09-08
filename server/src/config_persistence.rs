use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use poise_application::{
    ConfigurationApplicationPersistence, ConfigurationApplicationRequest,
    ConfigurationApplicationStatus, ConfigurationApplicationStore,
    ConfigurationApplicationStoreError, StoredTrackActivity, TrackActivityKind,
};
use poise_core::track::TrackId;
use poise_protocol::TrackConfigurationView;
use poise_storage::sqlite::{
    ConfigurationApplicationAuditRow, ConfigurationApplicationAuditStatus, SqliteStorage,
};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::config_projection::configuration_view_from_definition;

/// The instance-wide lock covers version observation, audit insertion, file
/// replacement and terminal audit updates. Configuration is one instance fact,
/// not a per-track file.
pub(crate) struct ConfigurationApplicationStoreAdapter {
    path: PathBuf,
    storage: Arc<SqliteStorage>,
    lock: Mutex<()>,
    committed_raw: StdMutex<String>,
}

impl ConfigurationApplicationStoreAdapter {
    pub(crate) fn new(path: PathBuf, storage: Arc<SqliteStorage>) -> Result<Self> {
        let committed_raw =
            fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        reconcile_interrupted_applications(&storage, &committed_raw)?;
        Ok(Self {
            path,
            storage,
            lock: Mutex::new(()),
            committed_raw: StdMutex::new(committed_raw),
        })
    }

    pub(crate) fn latest_for_track(
        &self,
        track_id: &str,
    ) -> std::result::Result<
        Option<ConfigurationApplicationAuditRow>,
        ConfigurationApplicationStoreError,
    > {
        self.storage
            .load_latest_configuration_application_for_track(track_id)
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))
    }

    pub(crate) fn active_application(
        &self,
    ) -> std::result::Result<
        Option<ConfigurationApplicationAuditRow>,
        ConfigurationApplicationStoreError,
    > {
        self.storage
            .load_active_configuration_application()
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))
    }

    pub(crate) fn application_by_id(
        &self,
        application_id: &str,
    ) -> std::result::Result<
        Option<ConfigurationApplicationAuditRow>,
        ConfigurationApplicationStoreError,
    > {
        self.storage
            .load_configuration_application(application_id)
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))
    }

    pub(crate) fn latest_external_configuration_conflict(
        &self,
    ) -> std::result::Result<
        Option<poise_storage::sqlite::ConfigurationApplicationExternalConflictRow>,
        ConfigurationApplicationStoreError,
    > {
        self.storage
            .load_latest_external_configuration_conflict()
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))
    }

    pub(crate) fn candidate_version(
        &self,
        track_id: &str,
        candidate: &TrackConfigurationView,
    ) -> std::result::Result<String, ConfigurationApplicationStoreError> {
        let input = fs::read_to_string(&self.path)
            .map_err(|error| ConfigurationApplicationStoreError::WriteFailed(error.to_string()))?;
        let patched = patch_track_configuration(&input, track_id, candidate)
            .map_err(|error| ConfigurationApplicationStoreError::WriteFailed(error.to_string()))?;
        canonical_version(&patched)
    }

    fn record_external_change(
        &self,
        track_id: &TrackId,
        request: &ConfigurationApplicationRequest,
        observed: &str,
        existing: Option<&ConfigurationApplicationAuditRow>,
    ) -> std::result::Result<(), ConfigurationApplicationStoreError> {
        let change_fields = configuration_change_fields(
            observed,
            track_id.as_str(),
            &configuration_view_from_definition(&request.candidate),
        )
        .unwrap_or_default();
        let record_activity = !external_change_already_recorded(existing);
        if existing.is_some() {
            self.storage
                .record_configuration_application_failure(
                    &request.application_id,
                    "external_change",
                    Utc::now(),
                )
                .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?;
        } else {
            let now = Utc::now();
            self.storage
                .record_configuration_application(&ConfigurationApplicationAuditRow {
                    application_id: request.application_id.clone(),
                    request_hash: request.request_hash.clone(),
                    track_id: track_id.as_str().to_string(),
                    old_version: request.expected_version.clone(),
                    candidate_version: request.candidate_version.clone(),
                    result: ConfigurationApplicationAuditStatus::Failed,
                    change_fields: Some(change_fields.clone()),
                    failure_reason: Some("external_change".to_string()),
                    created_at: now,
                    updated_at: now,
                })
                .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?;
        }
        if record_activity {
            record_activity_best_effort(
                &self.storage,
                track_id,
                &request.application_id,
                TrackActivityKind::ConfigurationFailed,
                change_fields,
            );
        }
        Ok(())
    }
}

fn external_change_already_recorded(existing: Option<&ConfigurationApplicationAuditRow>) -> bool {
    matches!(
        existing,
        Some(ConfigurationApplicationAuditRow {
            result: ConfigurationApplicationAuditStatus::Failed,
            failure_reason: Some(reason),
            ..
        }) if reason == "external_change"
    )
}

/// Activity history is diagnostic only: its failure must not alter the durable
/// configuration audit or the file-application outcome.
fn record_activity_best_effort(
    storage: &SqliteStorage,
    track_id: &TrackId,
    application_id: &str,
    kind: TrackActivityKind,
    change_fields: Vec<String>,
) {
    if let Err(error) = storage.record_track_activity(&StoredTrackActivity {
        id: 0,
        track_id: track_id.clone(),
        kind,
        reference_id: Some(application_id.to_string()),
        change_fields,
        created_at: Utc::now(),
    }) {
        tracing::warn!(
            track_id = track_id.as_str(),
            application_id,
            error = %error,
            "failed to record configuration activity"
        );
    }
}

/// A process never resumes a configuration workflow. Its durable audit is
/// reconciled with the one durable configuration fact before serving requests.
fn reconcile_interrupted_applications(storage: &SqliteStorage, current: &str) -> Result<()> {
    let current_version = canonical_version(current).map_err(anyhow::Error::new)?;
    for row in storage.load_active_configuration_applications()? {
        let result = if row.candidate_version == current_version {
            ConfigurationApplicationAuditStatus::Active
        } else if row.old_version == current_version {
            ConfigurationApplicationAuditStatus::Interrupted
        } else {
            ConfigurationApplicationAuditStatus::Failed
        };
        storage.update_configuration_application_result(&row.application_id, result, Utc::now())?;
        let track_id = TrackId::new(row.track_id);
        record_activity_best_effort(
            storage,
            &track_id,
            &row.application_id,
            configuration_activity_kind(result),
            row.change_fields.unwrap_or_default(),
        );
    }
    Ok(())
}

fn canonical_version(
    input: &str,
) -> std::result::Result<String, ConfigurationApplicationStoreError> {
    let config = crate::config::parse_config(input)
        .map_err(|error| ConfigurationApplicationStoreError::WriteFailed(error.to_string()))?;
    let mut configurations = config
        .track_definitions()
        .map_err(|error| ConfigurationApplicationStoreError::WriteFailed(error.to_string()))?
        .into_iter()
        .map(|definition| {
            (
                definition.track_id().as_str().to_string(),
                configuration_view_from_definition(&definition),
            )
        })
        .collect::<Vec<_>>();
    configurations.sort_by(|left, right| left.0.cmp(&right.0));
    let payload = serde_json::to_vec(&configurations)
        .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(payload)))
}

fn audit_status(
    status: ConfigurationApplicationStatus,
) -> std::result::Result<ConfigurationApplicationAuditStatus, ConfigurationApplicationStoreError> {
    match status {
        ConfigurationApplicationStatus::Idle => Err(ConfigurationApplicationStoreError::Internal(
            "idle is not a durable configuration application status".to_string(),
        )),
        ConfigurationApplicationStatus::Preparing => {
            Ok(ConfigurationApplicationAuditStatus::Preparing)
        }
        ConfigurationApplicationStatus::WaitingForOrders => {
            Ok(ConfigurationApplicationAuditStatus::WaitingForOrders)
        }
        ConfigurationApplicationStatus::Activating => {
            Ok(ConfigurationApplicationAuditStatus::Activating)
        }
        ConfigurationApplicationStatus::Active => Ok(ConfigurationApplicationAuditStatus::Active),
        ConfigurationApplicationStatus::Failed => Ok(ConfigurationApplicationAuditStatus::Failed),
    }
}

fn configuration_change_fields(
    input: &str,
    track_id: &str,
    candidate: &TrackConfigurationView,
) -> Result<Vec<String>> {
    let definition = crate::config::parse_config(input)?
        .track_definitions()?
        .into_iter()
        .find(|definition| definition.track_id().as_str() == track_id)
        .context("track not found in configuration")?;
    let current = configuration_view_from_definition(&definition);
    let mut fields = Vec::new();
    for (name, changed) in [
        ("lower_price", current.lower_price != candidate.lower_price),
        ("upper_price", current.upper_price != candidate.upper_price),
        (
            "long_exposure_units",
            current.long_exposure_units != candidate.long_exposure_units,
        ),
        (
            "short_exposure_units",
            current.short_exposure_units != candidate.short_exposure_units,
        ),
        (
            "notional_per_unit",
            current.notional_per_unit != candidate.notional_per_unit,
        ),
        (
            "min_rebalance_units",
            current.min_rebalance_units != candidate.min_rebalance_units,
        ),
        (
            "shape_family",
            current.shape_family != candidate.shape_family,
        ),
        (
            "out_of_band_policy",
            current.out_of_band_policy != candidate.out_of_band_policy,
        ),
        (
            "max_notional",
            current.max_notional != candidate.max_notional,
        ),
        (
            "daily_loss_limit",
            current.daily_loss_limit != candidate.daily_loss_limit,
        ),
        (
            "total_loss_limit",
            current.total_loss_limit != candidate.total_loss_limit,
        ),
        (
            "risk_acquisition.initial_ratio",
            current.risk_acquisition.initial_ratio != candidate.risk_acquisition.initial_ratio,
        ),
        (
            "risk_acquisition.advantage_steps",
            current.risk_acquisition.advantage_steps != candidate.risk_acquisition.advantage_steps,
        ),
        (
            "risk_acquisition.min_release_steps",
            current.risk_acquisition.min_release_steps
                != candidate.risk_acquisition.min_release_steps,
        ),
        (
            "risk_acquisition.max_release_steps",
            current.risk_acquisition.max_release_steps
                != candidate.risk_acquisition.max_release_steps,
        ),
        (
            "risk_acquisition.catchup_ratio",
            current.risk_acquisition.catchup_ratio != candidate.risk_acquisition.catchup_ratio,
        ),
        (
            "risk_acquisition.stale_release_minutes",
            current.risk_acquisition.stale_release_minutes
                != candidate.risk_acquisition.stale_release_minutes,
        ),
    ] {
        if changed {
            fields.push(name.to_string());
        }
    }
    Ok(fields)
}

#[async_trait]
impl ConfigurationApplicationStore for ConfigurationApplicationStoreAdapter {
    async fn accept_configuration_application(
        &self,
        track_id: &TrackId,
        request: &ConfigurationApplicationRequest,
    ) -> std::result::Result<ConfigurationApplicationPersistence, ConfigurationApplicationStoreError>
    {
        let _guard = self.lock.lock().await;
        let original = fs::read_to_string(&self.path)
            .map_err(|error| ConfigurationApplicationStoreError::WriteFailed(error.to_string()))?;
        let existing = self
            .storage
            .load_configuration_application(&request.application_id)
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?;
        if let Some(existing) = existing.as_ref() {
            if existing.track_id != track_id.as_str()
                || existing.request_hash != request.request_hash
            {
                return Err(ConfigurationApplicationStoreError::ApplicationIdConflict);
            }
            if !matches!(
                existing.result,
                ConfigurationApplicationAuditStatus::Interrupted
                    | ConfigurationApplicationAuditStatus::Failed
            ) {
                return Ok(ConfigurationApplicationPersistence::AlreadyPersisted);
            }
        }
        if original
            != *self.committed_raw.lock().map_err(|_| {
                ConfigurationApplicationStoreError::Internal(
                    "configuration baseline lock poisoned".to_string(),
                )
            })?
        {
            self.record_external_change(track_id, request, &original, existing.as_ref())?;
            return Err(ConfigurationApplicationStoreError::ExternalChange);
        }
        if canonical_version(&original)? != request.expected_version {
            return Err(ConfigurationApplicationStoreError::VersionConflict);
        }
        if let Some(active) = self
            .storage
            .load_active_configuration_application()
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?
            && active.application_id != request.application_id
        {
            return Err(ConfigurationApplicationStoreError::ApplicationIdConflict);
        }
        if existing.is_some() {
            let change_fields = existing
                .as_ref()
                .and_then(|row| row.change_fields.clone())
                .unwrap_or_default();
            self.storage
                .update_configuration_application_result(
                    &request.application_id,
                    ConfigurationApplicationAuditStatus::Preparing,
                    Utc::now(),
                )
                .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?;
            record_activity_best_effort(
                &self.storage,
                track_id,
                &request.application_id,
                TrackActivityKind::ConfigurationPreparing,
                change_fields,
            );
            return Ok(ConfigurationApplicationPersistence::Persisted);
        }
        let now = Utc::now();
        let change_fields = configuration_change_fields(
            &original,
            track_id.as_str(),
            &configuration_view_from_definition(&request.candidate),
        )
        .map_err(|error| ConfigurationApplicationStoreError::WriteFailed(error.to_string()))?;
        self.storage
            .record_configuration_application(&ConfigurationApplicationAuditRow {
                application_id: request.application_id.clone(),
                request_hash: request.request_hash.clone(),
                track_id: track_id.as_str().to_string(),
                old_version: request.expected_version.clone(),
                candidate_version: request.candidate_version.clone(),
                result: ConfigurationApplicationAuditStatus::Preparing,
                change_fields: Some(change_fields.clone()),
                failure_reason: None,
                created_at: now,
                updated_at: now,
            })
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?;
        record_activity_best_effort(
            &self.storage,
            track_id,
            &request.application_id,
            TrackActivityKind::ConfigurationPreparing,
            change_fields,
        );
        Ok(ConfigurationApplicationPersistence::Persisted)
    }

    async fn persist_accepted_configuration(
        &self,
        track_id: &TrackId,
        request: &ConfigurationApplicationRequest,
    ) -> std::result::Result<ConfigurationApplicationPersistence, ConfigurationApplicationStoreError>
    {
        let _guard = self.lock.lock().await;
        let existing = self
            .storage
            .load_configuration_application(&request.application_id)
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?
            .ok_or_else(|| {
                ConfigurationApplicationStoreError::Internal(
                    "configuration application was not accepted".to_string(),
                )
            })?;
        if existing.track_id != track_id.as_str() || existing.request_hash != request.request_hash {
            return Err(ConfigurationApplicationStoreError::ApplicationIdConflict);
        }
        let original = fs::read_to_string(&self.path)
            .map_err(|error| ConfigurationApplicationStoreError::WriteFailed(error.to_string()))?;
        if original
            != *self.committed_raw.lock().map_err(|_| {
                ConfigurationApplicationStoreError::Internal(
                    "configuration baseline lock poisoned".to_string(),
                )
            })?
        {
            self.record_external_change(track_id, request, &original, Some(&existing))?;
            return Err(ConfigurationApplicationStoreError::ExternalChange);
        }
        if canonical_version(&original)? != request.expected_version {
            return Err(ConfigurationApplicationStoreError::VersionConflict);
        }
        let candidate = configuration_view_from_definition(&request.candidate);
        let patched = patch_track_configuration(&original, track_id.as_str(), &candidate)
            .map_err(|error| ConfigurationApplicationStoreError::WriteFailed(error.to_string()))?;
        if canonical_version(&patched)? != request.candidate_version {
            return Err(ConfigurationApplicationStoreError::Internal(
                "candidate configuration version mismatch".to_string(),
            ));
        }
        if let Err(error) =
            patch_file_atomically_if_unchanged(&self.path, &original, track_id.as_str(), &candidate)
        {
            if error.to_string().contains("changed externally") {
                self.record_external_change(track_id, request, &original, Some(&existing))?;
                return Err(ConfigurationApplicationStoreError::ExternalChange);
            }
            return Err(ConfigurationApplicationStoreError::WriteFailed(
                error.to_string(),
            ));
        }
        *self.committed_raw.lock().map_err(|_| {
            ConfigurationApplicationStoreError::Internal(
                "configuration baseline lock poisoned".to_string(),
            )
        })? = patched;
        Ok(ConfigurationApplicationPersistence::Persisted)
    }

    async fn configuration_application_status(
        &self,
        track_id: &TrackId,
    ) -> std::result::Result<ConfigurationApplicationStatus, ConfigurationApplicationStoreError>
    {
        let _guard = self.lock.lock().await;
        match self.latest_for_track(track_id.as_str())? {
            None => Ok(ConfigurationApplicationStatus::Idle),
            Some(row) => Ok(match row.result {
                ConfigurationApplicationAuditStatus::Preparing => {
                    ConfigurationApplicationStatus::Preparing
                }
                ConfigurationApplicationAuditStatus::WaitingForOrders => {
                    ConfigurationApplicationStatus::WaitingForOrders
                }
                ConfigurationApplicationAuditStatus::Activating => {
                    ConfigurationApplicationStatus::Activating
                }
                ConfigurationApplicationAuditStatus::Active => {
                    ConfigurationApplicationStatus::Active
                }
                ConfigurationApplicationAuditStatus::Failed
                | ConfigurationApplicationAuditStatus::Interrupted => {
                    ConfigurationApplicationStatus::Failed
                }
            }),
        }
    }

    async fn update_configuration_application_status(
        &self,
        track_id: &TrackId,
        request: &ConfigurationApplicationRequest,
        status: ConfigurationApplicationStatus,
    ) -> std::result::Result<(), ConfigurationApplicationStoreError> {
        let _guard = self.lock.lock().await;
        let next = audit_status(status)?;
        let previous = self
            .storage
            .load_configuration_application(&request.application_id)
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?
            .ok_or_else(|| {
                ConfigurationApplicationStoreError::Internal(
                    "configuration application was not accepted".to_string(),
                )
            })?;
        self.storage
            .update_configuration_application_result(&request.application_id, next, Utc::now())
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?;
        if previous.result != next {
            record_activity_best_effort(
                &self.storage,
                track_id,
                &request.application_id,
                configuration_activity_kind(next),
                previous.change_fields.unwrap_or_default(),
            );
        }
        Ok(())
    }
}

fn configuration_activity_kind(status: ConfigurationApplicationAuditStatus) -> TrackActivityKind {
    match status {
        ConfigurationApplicationAuditStatus::Preparing => TrackActivityKind::ConfigurationPreparing,
        ConfigurationApplicationAuditStatus::WaitingForOrders => {
            TrackActivityKind::ConfigurationWaitingForOrders
        }
        ConfigurationApplicationAuditStatus::Activating => {
            TrackActivityKind::ConfigurationActivating
        }
        ConfigurationApplicationAuditStatus::Active => TrackActivityKind::ConfigurationActive,
        ConfigurationApplicationAuditStatus::Failed => TrackActivityKind::ConfigurationFailed,
        ConfigurationApplicationAuditStatus::Interrupted => {
            TrackActivityKind::ConfigurationInterrupted
        }
    }
}

pub(crate) fn patch_track_configuration(
    input: &str,
    track_id: &str,
    candidate: &TrackConfigurationView,
) -> Result<String> {
    let mut document = input
        .parse::<toml_edit::DocumentMut>()
        .context("invalid TOML configuration")?;
    let tracks = document["tracks"]
        .as_array_of_tables_mut()
        .context("missing tracks array")?;
    let track = tracks
        .iter_mut()
        .find(|track| track["track_id"].as_str() == Some(track_id))
        .context("target track not found")?;
    track["lower_price"] = toml_edit::value(candidate.lower_price);
    track["upper_price"] = toml_edit::value(candidate.upper_price);
    track["long_exposure_units"] = toml_edit::value(candidate.long_exposure_units);
    track["short_exposure_units"] = toml_edit::value(candidate.short_exposure_units);
    track["notional_per_unit"] = toml_edit::value(candidate.notional_per_unit);
    track["min_rebalance_units"] = toml_edit::value(candidate.min_rebalance_units);
    track["max_notional"] = toml_edit::value(candidate.max_notional);
    track["daily_loss_limit"] = toml_edit::value(candidate.daily_loss_limit);
    track["total_loss_limit"] = toml_edit::value(candidate.total_loss_limit);
    track["shape_family"] = toml_edit::value(candidate.shape_family.to_string());
    let policy = format!(
        "value = {}",
        out_of_band_policy_toml(&candidate.out_of_band_policy)
    );
    let policy = policy.parse::<toml_edit::DocumentMut>()?;
    track["out_of_band_policy"] = policy["value"].clone();
    track["risk_acquisition"]["initial_ratio"] =
        toml_edit::value(candidate.risk_acquisition.initial_ratio);
    track["risk_acquisition"]["advantage_steps"] =
        toml_edit::value(candidate.risk_acquisition.advantage_steps);
    track["risk_acquisition"]["min_release_steps"] =
        toml_edit::value(candidate.risk_acquisition.min_release_steps);
    track["risk_acquisition"]["max_release_steps"] =
        toml_edit::value(candidate.risk_acquisition.max_release_steps);
    track["risk_acquisition"]["catchup_ratio"] =
        toml_edit::value(candidate.risk_acquisition.catchup_ratio);
    track["risk_acquisition"]["stale_release_minutes"] =
        toml_edit::value(candidate.risk_acquisition.stale_release_minutes);
    Ok(document.to_string())
}

fn out_of_band_policy_toml(policy: &poise_protocol::BandProtectionPolicy) -> String {
    match policy {
        poise_protocol::BandProtectionPolicy::Freeze => "\"freeze\"".to_string(),
        poise_protocol::BandProtectionPolicy::Terminate => "\"terminate\"".to_string(),
        poise_protocol::BandProtectionPolicy::Flatten { trigger, recover } => format!(
            "{{ flatten = {{ trigger = {}, recover = {} }} }}",
            trigger_toml(trigger),
            recover_toml(recover)
        ),
    }
}
fn trigger_toml(value: &poise_protocol::BandFlattenTrigger) -> String {
    match value {
        poise_protocol::BandFlattenTrigger::Immediate => "\"immediate\"".to_string(),
        poise_protocol::BandFlattenTrigger::FlattenConfirm { bps } => {
            format!("{{ flatten_confirm = {{ bps = {bps} }} }}")
        }
    }
}
fn recover_toml(value: &poise_protocol::BandRecoverPolicy) -> String {
    match value {
        poise_protocol::BandRecoverPolicy::BackInBand => "\"back_in_band\"".to_string(),
        poise_protocol::BandRecoverPolicy::ReentryConfirm { bps } => {
            format!("{{ reentry_confirm = {{ bps = {bps} }} }}")
        }
    }
}

#[allow(dead_code)]
pub(crate) fn patch_file_atomically(
    path: &Path,
    track_id: &str,
    candidate: &TrackConfigurationView,
) -> Result<()> {
    let original = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let patched = patch_track_configuration(&original, track_id, candidate)?;
    let permissions = fs::metadata(path)?.permissions();
    let parent = path.parent().context("configuration path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.as_file_mut().set_permissions(permissions)?;
    temporary.write_all(patched.as_bytes())?;
    temporary.as_file_mut().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[allow(dead_code)]
pub(crate) fn patch_file_atomically_if_unchanged(
    path: &Path,
    expected_original: &str,
    track_id: &str,
    candidate: &TrackConfigurationView,
) -> Result<()> {
    patch_file_atomically_if_unchanged_with_hook(
        path,
        expected_original,
        track_id,
        candidate,
        || {},
    )
}

fn patch_file_atomically_if_unchanged_with_hook(
    path: &Path,
    expected_original: &str,
    track_id: &str,
    candidate: &TrackConfigurationView,
    before_final_check: impl FnOnce(),
) -> Result<()> {
    if fs::read_to_string(path)? != expected_original {
        anyhow::bail!("configuration changed externally");
    }

    // Construct the replacement from the accepted snapshot, never from a
    // later read. An external writer can still race the final read and rename;
    // ordinary filesystem replacement has no cross-process compare-and-swap.
    let patched = patch_track_configuration(expected_original, track_id, candidate)?;
    let permissions = fs::metadata(path)?.permissions();
    let parent = path.parent().context("configuration path has no parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.as_file_mut().set_permissions(permissions)?;
    temporary.write_all(patched.as_bytes())?;
    temporary.as_file_mut().sync_all()?;

    before_final_check();
    if fs::read_to_string(path)? != expected_original {
        anyhow::bail!("configuration changed externally");
    }
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigurationApplicationStoreAdapter, canonical_version, external_change_already_recorded,
        patch_file_atomically, patch_file_atomically_if_unchanged,
        patch_file_atomically_if_unchanged_with_hook, patch_track_configuration,
    };
    use chrono::Utc;
    use poise_application::{
        ConfigurationApplicationPersistence, ConfigurationApplicationRequest,
        ConfigurationApplicationStore,
    };
    use poise_protocol::{
        BandFlattenTrigger, BandProtectionPolicy, BandRecoverPolicy, RiskAcquisitionConfigView,
        ShapeFamily, TrackConfigurationView,
    };
    use poise_storage::sqlite::{
        ConfigurationApplicationAuditRow, ConfigurationApplicationAuditStatus, SqliteStorage,
    };
    use std::sync::Arc;

    #[test]
    fn canonical_version_ignores_credentials_and_comments() {
        let first = "# first\n[exchange]\nvenue = 'binance'\napi_key = 'secret-a'\napi_secret = 'secret'\n[[tracks]]\ntrack_id='btc'\nsymbol='BTCUSDT'\nlower_price=90.0\nupper_price=110.0\nlong_exposure_units=1.0\nshort_exposure_units=1.0\nnotional_per_unit=1.0\ndaily_loss_limit=1.0\ntotal_loss_limit=1.0\n";
        let second = first
            .replace("# first", "# externally edited comment")
            .replace("secret-a", "secret-b");
        assert_eq!(
            canonical_version(first).unwrap(),
            canonical_version(&second).unwrap()
        );
    }

    #[test]
    fn only_an_already_recorded_external_change_deduplicates_failure_activity() {
        let row = |failure_reason: Option<&str>| ConfigurationApplicationAuditRow {
            application_id: "apply".to_string(),
            request_hash: "hash".to_string(),
            track_id: "btc".to_string(),
            old_version: "old".to_string(),
            candidate_version: "candidate".to_string(),
            result: ConfigurationApplicationAuditStatus::Failed,
            change_fields: None,
            failure_reason: failure_reason.map(str::to_string),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        assert!(!external_change_already_recorded(Some(&row(Some(
            "write_failed"
        )))));
        assert!(external_change_already_recorded(Some(&row(Some(
            "external_change"
        )))));
    }

    #[test]
    fn reopening_marks_previously_accepted_candidate_as_active() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let current = "[exchange]\nvenue='binance'\n[[tracks]]\ntrack_id='btc'\nsymbol='BTCUSDT'\nlower_price=90.0\nupper_price=110.0\nlong_exposure_units=1.0\nshort_exposure_units=1.0\nnotional_per_unit=1.0\ndaily_loss_limit=1.0\ntotal_loss_limit=1.0\n";
        std::fs::write(&path, current).unwrap();
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let now = Utc::now();
        storage
            .record_configuration_application(&ConfigurationApplicationAuditRow {
                application_id: "apply-restart-active".to_string(),
                request_hash: "hash".to_string(),
                track_id: "btc".to_string(),
                old_version: "old".to_string(),
                candidate_version: canonical_version(current).unwrap(),
                result: ConfigurationApplicationAuditStatus::Preparing,
                change_fields: None,
                failure_reason: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();

        let _adapter =
            ConfigurationApplicationStoreAdapter::new(path, Arc::clone(&storage)).unwrap();

        assert_eq!(
            storage
                .load_configuration_application("apply-restart-active")
                .unwrap()
                .unwrap()
                .result,
            ConfigurationApplicationAuditStatus::Active
        );
    }

    #[test]
    fn reopening_marks_unpersisted_application_interrupted() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let current = "[exchange]\nvenue='binance'\n[[tracks]]\ntrack_id='btc'\nsymbol='BTCUSDT'\nlower_price=90.0\nupper_price=110.0\nlong_exposure_units=1.0\nshort_exposure_units=1.0\nnotional_per_unit=1.0\ndaily_loss_limit=1.0\ntotal_loss_limit=1.0\n";
        std::fs::write(&path, current).unwrap();
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let now = Utc::now();
        storage
            .record_configuration_application(&ConfigurationApplicationAuditRow {
                application_id: "apply-restart-interrupted".to_string(),
                request_hash: "hash".to_string(),
                track_id: "btc".to_string(),
                old_version: canonical_version(current).unwrap(),
                candidate_version: "candidate".to_string(),
                result: ConfigurationApplicationAuditStatus::WaitingForOrders,
                change_fields: None,
                failure_reason: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();

        let _adapter =
            ConfigurationApplicationStoreAdapter::new(path, Arc::clone(&storage)).unwrap();

        assert_eq!(
            storage
                .load_configuration_application("apply-restart-interrupted")
                .unwrap()
                .unwrap()
                .result,
            ConfigurationApplicationAuditStatus::Interrupted
        );
    }

    #[tokio::test]
    async fn same_id_retries_a_pre_file_failure_when_current_configuration_is_still_old() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let current = "[exchange]\nvenue='binance'\n[[tracks]]\ntrack_id='btc'\nsymbol='BTCUSDT'\nlower_price=90.0\nupper_price=110.0\nlong_exposure_units=1.0\nshort_exposure_units=1.0\nnotional_per_unit=1.0\ndaily_loss_limit=1.0\ntotal_loss_limit=1.0\n";
        std::fs::write(&path, current).unwrap();
        let storage = Arc::new(SqliteStorage::in_memory().unwrap());
        let version = canonical_version(current).unwrap();
        let now = Utc::now();
        storage
            .record_configuration_application(&ConfigurationApplicationAuditRow {
                application_id: "apply-retry-failed".to_string(),
                request_hash: "hash".to_string(),
                track_id: "btc".to_string(),
                old_version: version.clone(),
                candidate_version: version.clone(),
                result: ConfigurationApplicationAuditStatus::Failed,
                change_fields: None,
                failure_reason: None,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
        let adapter =
            ConfigurationApplicationStoreAdapter::new(path, Arc::clone(&storage)).unwrap();
        let definition = crate::config::parse_config(current)
            .unwrap()
            .track_definitions()
            .unwrap()
            .pop()
            .unwrap();
        let request = ConfigurationApplicationRequest {
            application_id: "apply-retry-failed".to_string(),
            request_hash: "hash".to_string(),
            expected_version: version.clone(),
            candidate_version: version,
            candidate: definition,
        };

        assert_eq!(
            adapter
                .accept_configuration_application(&poise_core::track::TrackId::new("btc"), &request)
                .await
                .unwrap(),
            ConfigurationApplicationPersistence::Persisted
        );
        assert_eq!(
            storage
                .load_configuration_application("apply-retry-failed")
                .unwrap()
                .unwrap()
                .result,
            ConfigurationApplicationAuditStatus::Preparing
        );
    }

    #[test]
    fn patches_only_the_target_track_online_fields() {
        let input = "# keep\n[exchange]\nvenue = 'binance'\napi_key = 'secret'\n[[tracks]]\ntrack_id = 'btc'\nsymbol = 'BTCUSDT'\nleverage = 10\nlower_price = 90.0\nupper_price = 110.0\nlong_exposure_units = 8.0\nshort_exposure_units = 8.0\nnotional_per_unit = 100.0\ndaily_loss_limit = 10.0\ntotal_loss_limit = 20.0\n[[tracks]]\ntrack_id = 'eth'\nsymbol = 'ETHUSDT'\nlower_price=1\nupper_price=2\nlong_exposure_units=1\nshort_exposure_units=1\nnotional_per_unit=1\ndaily_loss_limit=1\ntotal_loss_limit=1\n";
        let candidate = TrackConfigurationView {
            lower_price: 80.0,
            upper_price: 120.0,
            long_exposure_units: 4.0,
            short_exposure_units: 5.0,
            notional_per_unit: 200.0,
            min_rebalance_units: 0.5,
            shape_family: ShapeFamily::Responsive,
            out_of_band_policy: BandProtectionPolicy::Flatten {
                trigger: BandFlattenTrigger::FlattenConfirm { bps: 120 },
                recover: BandRecoverPolicy::ReentryConfirm { bps: 80 },
            },
            max_notional: 900.0,
            daily_loss_limit: 11.0,
            total_loss_limit: 22.0,
            risk_acquisition: RiskAcquisitionConfigView {
                initial_ratio: 0.5,
                advantage_steps: 2.0,
                min_release_steps: 1.0,
                max_release_steps: 4.0,
                catchup_ratio: 0.25,
                stale_release_minutes: 60.0,
            },
        };
        let before = crate::config::parse_config(input).unwrap();
        let output = patch_track_configuration(input, "btc", &candidate).unwrap();
        assert!(output.contains("# keep"));
        assert!(output.contains("api_key = 'secret'"));
        assert!(output.contains("track_id = 'eth'"));
        assert!(output.contains("leverage = 10"));
        assert!(output.contains("lower_price = 80.0"));
        let config = crate::config::parse_config(&output).unwrap();
        let btc = &config.tracks[0];
        assert_eq!(btc.lower_price, 80.0);
        assert_eq!(btc.upper_price, 120.0);
        assert_eq!(btc.long_exposure_units, 4.0);
        assert_eq!(btc.short_exposure_units, 5.0);
        assert_eq!(btc.notional_per_unit, 200.0);
        assert_eq!(btc.min_rebalance_units, Some(0.5));
        assert_eq!(
            btc.shape_family,
            Some(poise_core::strategy::ShapeFamily::Responsive)
        );
        assert_eq!(btc.max_notional, Some(900.0));
        assert_eq!(btc.daily_loss_limit, 11.0);
        assert_eq!(btc.total_loss_limit, 22.0);
        assert_eq!(btc.risk_acquisition.initial_ratio, 0.5);
        assert_eq!(btc.risk_acquisition.advantage_steps, 2.0);
        assert_eq!(btc.risk_acquisition.min_release_steps, 1.0);
        assert_eq!(btc.risk_acquisition.max_release_steps, 4.0);
        assert_eq!(btc.risk_acquisition.catchup_ratio, 0.25);
        assert_eq!(btc.risk_acquisition.stale_release_minutes, 60.0);
        assert_eq!(config.tracks[1], before.tracks[1]);
        assert!(matches!(
            btc.out_of_band_policy,
            Some(poise_core::strategy::BandProtectionPolicy::Flatten {
                trigger: poise_core::strategy::BandFlattenTrigger::FlattenConfirm { bps: 120 },
                recover: poise_core::strategy::BandRecoverPolicy::ReentryConfirm { bps: 80 }
            })
        ));
    }

    #[test]
    fn rejects_external_change_without_replacing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "[[tracks]]\ntrack_id='btc'\nlower_price=90.0\nupper_price=110.0\nlong_exposure_units=1.0\nshort_exposure_units=1.0\nnotional_per_unit=1.0\ndaily_loss_limit=1.0\ntotal_loss_limit=1.0\n";
        std::fs::write(&path, "externally changed").unwrap();
        let candidate: poise_protocol::TrackConfigurationView = serde_json::from_str(r#"{"lower_price":80.0,"upper_price":120.0,"long_exposure_units":4.0,"short_exposure_units":5.0,"notional_per_unit":200.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":"freeze","max_notional":900.0,"daily_loss_limit":11.0,"total_loss_limit":22.0,"risk_acquisition":{"initial_ratio":0.5,"advantage_steps":2.0,"min_release_steps":1.0,"max_release_steps":4.0,"catchup_ratio":0.25,"stale_release_minutes":60.0}}"#).unwrap();
        assert!(patch_file_atomically_if_unchanged(&path, original, "btc", &candidate).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "externally changed");
    }

    #[test]
    fn does_not_merge_a_change_that_arrives_before_the_final_check() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "[[tracks]]\ntrack_id='btc'\nlower_price=90.0\nupper_price=110.0\nlong_exposure_units=1.0\nshort_exposure_units=1.0\nnotional_per_unit=1.0\ndaily_loss_limit=1.0\ntotal_loss_limit=1.0\n";
        std::fs::write(&path, original).unwrap();
        let candidate: TrackConfigurationView = serde_json::from_str(r#"{"lower_price":80.0,"upper_price":120.0,"long_exposure_units":4.0,"short_exposure_units":5.0,"notional_per_unit":200.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":"freeze","max_notional":900.0,"daily_loss_limit":11.0,"total_loss_limit":22.0,"risk_acquisition":{"initial_ratio":0.5,"advantage_steps":2.0,"min_release_steps":1.0,"max_release_steps":4.0,"catchup_ratio":0.25,"stale_release_minutes":60.0}}"#).unwrap();

        let result = patch_file_atomically_if_unchanged_with_hook(
            &path,
            original,
            "btc",
            &candidate,
            || std::fs::write(&path, format!("# external edit\n{original}")).unwrap(),
        );

        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("# external edit\n{original}")
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_patch_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        let original = "[exchange]\nvenue='binance'\napi_key='key'\napi_secret='secret'\n[[tracks]]\ntrack_id='btc'\nsymbol='BTCUSDT'\nlower_price=90.0\nupper_price=110.0\nlong_exposure_units=1.0\nshort_exposure_units=1.0\nnotional_per_unit=1.0\ndaily_loss_limit=1.0\ntotal_loss_limit=1.0\n";
        std::fs::write(&path, original).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let candidate: TrackConfigurationView = serde_json::from_str(r#"{"lower_price":80.0,"upper_price":120.0,"long_exposure_units":4.0,"short_exposure_units":5.0,"notional_per_unit":200.0,"min_rebalance_units":0.5,"shape_family":"linear","out_of_band_policy":"freeze","max_notional":900.0,"daily_loss_limit":11.0,"total_loss_limit":22.0,"risk_acquisition":{"initial_ratio":0.5,"advantage_steps":2.0,"min_release_steps":1.0,"max_release_steps":4.0,"catchup_ratio":0.25,"stale_release_minutes":60.0}}"#).unwrap();
        patch_file_atomically(&path, "btc", &candidate).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
