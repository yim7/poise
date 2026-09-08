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
    ConfigurationApplicationStoreError,
};
use poise_core::track::TrackId;
use poise_protocol::TrackConfigurationView;
use poise_storage::sqlite::{ConfigurationApplicationAuditRow, SqliteStorage};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

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
}

/// A process never resumes a configuration workflow. Its durable audit is
/// reconciled with the one durable configuration fact before serving requests.
fn reconcile_interrupted_applications(storage: &SqliteStorage, current: &str) -> Result<()> {
    let current_version = canonical_version(current).map_err(anyhow::Error::new)?;
    for row in storage.load_active_configuration_applications()? {
        let result = if row.candidate_version == current_version {
            "active"
        } else if row.old_version == current_version {
            "interrupted"
        } else {
            "failed"
        };
        storage.update_configuration_application_result(&row.application_id, result, Utc::now())?;
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

fn status_text(status: ConfigurationApplicationStatus) -> &'static str {
    match status {
        ConfigurationApplicationStatus::Idle => "idle",
        ConfigurationApplicationStatus::Preparing => "preparing",
        ConfigurationApplicationStatus::WaitingForOrders => "waiting_for_orders",
        ConfigurationApplicationStatus::Activating => "activating",
        ConfigurationApplicationStatus::Active => "active",
        ConfigurationApplicationStatus::Failed => "failed",
    }
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
            if !matches!(existing.result.as_str(), "interrupted" | "failed") {
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
            self.storage
                .update_configuration_application_result(
                    &request.application_id,
                    status_text(ConfigurationApplicationStatus::Preparing),
                    Utc::now(),
                )
                .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?;
            return Ok(ConfigurationApplicationPersistence::Persisted);
        }
        let now = Utc::now();
        self.storage
            .record_configuration_application(&ConfigurationApplicationAuditRow {
                application_id: request.application_id.clone(),
                request_hash: request.request_hash.clone(),
                track_id: track_id.as_str().to_string(),
                old_version: request.expected_version.clone(),
                candidate_version: request.candidate_version.clone(),
                result: status_text(ConfigurationApplicationStatus::Preparing).to_string(),
                created_at: now,
                updated_at: now,
            })
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))?;
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
        Ok(self
            .latest_for_track(track_id.as_str())?
            .map(|row| match row.result.as_str() {
                "preparing" => ConfigurationApplicationStatus::Preparing,
                "waiting_for_orders" => ConfigurationApplicationStatus::WaitingForOrders,
                "activating" => ConfigurationApplicationStatus::Activating,
                "active" => ConfigurationApplicationStatus::Active,
                "failed" => ConfigurationApplicationStatus::Failed,
                _ => ConfigurationApplicationStatus::Idle,
            })
            .unwrap_or(ConfigurationApplicationStatus::Idle))
    }

    async fn update_configuration_application_status(
        &self,
        _track_id: &TrackId,
        request: &ConfigurationApplicationRequest,
        status: ConfigurationApplicationStatus,
    ) -> std::result::Result<(), ConfigurationApplicationStoreError> {
        let _guard = self.lock.lock().await;
        self.storage
            .update_configuration_application_result(
                &request.application_id,
                status_text(status),
                Utc::now(),
            )
            .map_err(|error| ConfigurationApplicationStoreError::Internal(error.to_string()))
    }
}

fn configuration_view_from_definition(
    definition: &poise_core::track::TrackDefinition,
) -> TrackConfigurationView {
    let config = definition.track_config();
    TrackConfigurationView {
        lower_price: config.lower_price,
        upper_price: config.upper_price,
        long_exposure_units: config.long_exposure_units,
        short_exposure_units: config.short_exposure_units,
        notional_per_unit: config.notional_per_unit,
        min_rebalance_units: config.min_rebalance_units,
        shape_family: match config.shape_family {
            poise_core::strategy::ShapeFamily::Linear => poise_protocol::ShapeFamily::Linear,
            poise_core::strategy::ShapeFamily::Inertial => poise_protocol::ShapeFamily::Inertial,
            poise_core::strategy::ShapeFamily::Responsive => {
                poise_protocol::ShapeFamily::Responsive
            }
        },
        out_of_band_policy: match config.out_of_band_policy {
            poise_core::strategy::BandProtectionPolicy::Freeze => {
                poise_protocol::BandProtectionPolicy::Freeze
            }
            poise_core::strategy::BandProtectionPolicy::Terminate => {
                poise_protocol::BandProtectionPolicy::Terminate
            }
            poise_core::strategy::BandProtectionPolicy::Flatten { trigger, recover } => {
                poise_protocol::BandProtectionPolicy::Flatten {
                    trigger: match trigger {
                        poise_core::strategy::BandFlattenTrigger::Immediate => {
                            poise_protocol::BandFlattenTrigger::Immediate
                        }
                        poise_core::strategy::BandFlattenTrigger::FlattenConfirm { bps } => {
                            poise_protocol::BandFlattenTrigger::FlattenConfirm { bps }
                        }
                    },
                    recover: match recover {
                        poise_core::strategy::BandRecoverPolicy::BackInBand => {
                            poise_protocol::BandRecoverPolicy::BackInBand
                        }
                        poise_core::strategy::BandRecoverPolicy::ReentryConfirm { bps } => {
                            poise_protocol::BandRecoverPolicy::ReentryConfirm { bps }
                        }
                    },
                }
            }
        },
        max_notional: definition.max_notional(),
        daily_loss_limit: definition.loss_limits().daily_loss_limit,
        total_loss_limit: definition.loss_limits().total_loss_limit,
        risk_acquisition: poise_protocol::RiskAcquisitionConfigView {
            initial_ratio: config.risk_acquisition.initial_ratio,
            advantage_steps: config.risk_acquisition.advantage_steps,
            min_release_steps: config.risk_acquisition.min_release_steps,
            max_release_steps: config.risk_acquisition.max_release_steps,
            catchup_ratio: config.risk_acquisition.catchup_ratio,
            stale_release_minutes: config.risk_acquisition.stale_release_minutes,
        },
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
    let current = fs::read_to_string(path)?;
    if current != expected_original {
        anyhow::bail!("configuration changed externally");
    }
    patch_file_atomically(path, track_id, candidate)
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigurationApplicationStoreAdapter, canonical_version, patch_file_atomically,
        patch_file_atomically_if_unchanged, patch_track_configuration,
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
    use poise_storage::sqlite::{ConfigurationApplicationAuditRow, SqliteStorage};
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
                result: "preparing".to_string(),
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
            "active"
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
                result: "waiting_for_orders".to_string(),
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
            "interrupted"
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
                result: "failed".to_string(),
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
            "preparing"
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
