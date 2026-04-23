use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{SecondsFormat, Utc};
use poise_application::{
    AccountMonitorStore, ConfiguredTrackDefinition, PreparedTrackRegistry, TrackEffectStore,
    TrackMutationStore, TrackQueryStore,
};
use poise_engine::runtime::TrackRuntime;
#[cfg(test)]
use poise_engine::track::TrackId;
use poise_storage::sqlite::SqliteStorage;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateBootstrapMode {
    Strict,
    Rebuild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestedAction {
    RebuildState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PersistedStateMismatch {
    pub track_id: String,
    pub detail: PersistedStateMismatchDetail,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PersistedStateMismatchDetail {
    RestoreRevisionMismatch {
        expected_revision: poise_engine::persisted_runtime::TrackRestoreRevision,
        actual_revision: poise_engine::persisted_runtime::TrackRestoreRevision,
    },
    PersistedTrackMissingRuntime,
    PersistedTrackMissingFromConfig,
}

#[derive(Debug)]
pub enum StateBootstrapError {
    PersistedStateMismatch {
        db_path: PathBuf,
        mismatches: Vec<PersistedStateMismatch>,
        suggested_action: SuggestedAction,
    },
    Unexpected(anyhow::Error),
}

impl std::fmt::Display for StateBootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PersistedStateMismatch { db_path, .. } => {
                write!(
                    f,
                    "persisted state does not match current config in `{}`",
                    db_path.display()
                )
            }
            Self::Unexpected(error) => std::fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for StateBootstrapError {}

type BootstrapResult<T> = std::result::Result<T, StateBootstrapError>;

#[derive(Clone)]
pub struct StateRepositories {
    account_monitor_store: Option<Arc<dyn AccountMonitorStore>>,
    mutation_store: Arc<dyn TrackMutationStore>,
    query_store: Arc<dyn TrackQueryStore>,
    effect_store: Arc<dyn TrackEffectStore>,
}

pub struct PreparedStateStore {
    repositories: StateRepositories,
    prepared_registry: Arc<PreparedTrackRegistry>,
    db_path: PathBuf,
    rebuild_backup: Option<StateBackup>,
}

#[derive(Debug)]
struct StateBackup {
    moved_files: Vec<BackupFile>,
}

#[derive(Debug)]
struct BackupFile {
    live_path: PathBuf,
    backup_path: PathBuf,
}

impl PreparedStateStore {
    pub async fn run_startup<T, F, Fut>(mut self, startup: F) -> Result<T>
    where
        F: FnOnce(StateRepositories, Arc<PreparedTrackRegistry>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        match startup(
            self.repositories.clone(),
            Arc::clone(&self.prepared_registry),
        )
        .await
        {
            Ok(result) => {
                self.rebuild_backup = None;
                Ok(result)
            }
            Err(error) => Err(self.restore_after_failure(error)),
        }
    }

    pub fn restore_backup(&mut self) -> Result<()> {
        let Some(backup) = self.rebuild_backup.take() else {
            return Ok(());
        };
        remove_state_files(&self.db_path)?;
        backup.restore()
    }

    fn restore_after_failure(&mut self, error: anyhow::Error) -> anyhow::Error {
        match self.restore_backup() {
            Ok(()) => error,
            Err(restore_error) => error.context(format!(
                "also failed to restore rebuilt local state after startup failure: {restore_error}"
            )),
        }
    }

    #[cfg(test)]
    pub fn registry(&self) -> &PreparedTrackRegistry {
        &self.prepared_registry
    }
}

impl StateRepositories {
    #[cfg(test)]
    pub(crate) fn new<R>(repository: Arc<R>) -> Self
    where
        R: TrackMutationStore + TrackQueryStore + TrackEffectStore + 'static,
    {
        Self {
            account_monitor_store: None,
            mutation_store: repository.clone(),
            query_store: repository.clone(),
            effect_store: repository.clone(),
        }
    }

    pub(crate) fn from_sqlite_storage(repository: Arc<SqliteStorage>) -> Self {
        Self {
            account_monitor_store: Some(repository.clone()),
            mutation_store: repository.clone(),
            query_store: repository.clone(),
            effect_store: repository.clone(),
        }
    }

    pub fn account_monitor_store(&self) -> Option<Arc<dyn AccountMonitorStore>> {
        self.account_monitor_store.as_ref().map(Arc::clone)
    }

    pub fn mutation_store(&self) -> Arc<dyn TrackMutationStore> {
        Arc::clone(&self.mutation_store)
    }

    pub fn query_store(&self) -> Arc<dyn TrackQueryStore> {
        Arc::clone(&self.query_store)
    }

    pub fn effect_store(&self) -> Arc<dyn TrackEffectStore> {
        Arc::clone(&self.effect_store)
    }
}

impl StateBackup {
    fn restore(self) -> Result<()> {
        for moved in self.moved_files.iter().rev() {
            fs::rename(&moved.backup_path, &moved.live_path).with_context(|| {
                format!(
                    "failed to restore sqlite backup from `{}` to `{}`",
                    moved.backup_path.display(),
                    moved.live_path.display()
                )
            })?;
        }
        Ok(())
    }
}

fn unexpected(error: anyhow::Error) -> StateBootstrapError {
    StateBootstrapError::Unexpected(error)
}

pub async fn prepare_state_repository(
    config: &Config,
    db_path: &Path,
    mode: StateBootstrapMode,
) -> BootstrapResult<PreparedStateStore> {
    ensure_parent_dir(db_path).map_err(unexpected)?;
    let db_path = db_path.to_path_buf();
    let prepared_registry = Arc::new(build_prepared_registry(config).map_err(unexpected)?);

    match mode {
        StateBootstrapMode::Strict => {
            let repository = SqliteStorage::new(&db_path).map_err(unexpected)?;
            let mismatches =
                detect_persisted_state_mismatches(prepared_registry.as_ref(), &repository)
                    .await
                    .map_err(unexpected)?;
            if mismatches.is_empty() {
                hydrate_query_ready_state(prepared_registry.as_ref(), &repository)
                    .await
                    .map_err(unexpected)?;
                let repository = Arc::new(repository);
                return Ok(PreparedStateStore {
                    repositories: StateRepositories::from_sqlite_storage(repository),
                    prepared_registry,
                    db_path,
                    rebuild_backup: None,
                });
            }

            Err(StateBootstrapError::PersistedStateMismatch {
                db_path,
                mismatches,
                suggested_action: SuggestedAction::RebuildState,
            })
        }
        StateBootstrapMode::Rebuild => {
            let rebuild_backup = backup_and_reset_state_db(&db_path).map_err(unexpected)?;
            let repository = match SqliteStorage::new(&db_path) {
                Ok(repository) => repository,
                Err(error) => {
                    if let Some(backup) = rebuild_backup {
                        remove_state_files(&db_path).map_err(unexpected)?;
                        backup.restore().map_err(unexpected)?;
                    }
                    return Err(unexpected(error));
                }
            };
            if let Err(error) =
                hydrate_query_ready_state(prepared_registry.as_ref(), &repository).await
            {
                if let Some(backup) = rebuild_backup {
                    remove_state_files(&db_path).map_err(unexpected)?;
                    backup.restore().map_err(unexpected)?;
                }
                return Err(unexpected(error));
            }
            let repository = Arc::new(repository);
            Ok(PreparedStateStore {
                repositories: StateRepositories::from_sqlite_storage(repository),
                prepared_registry,
                db_path,
                rebuild_backup,
            })
        }
    }
}

fn build_prepared_registry(config: &Config) -> Result<PreparedTrackRegistry> {
    let mut configured = Vec::with_capacity(config.tracks.len());
    for track in &config.tracks {
        configured.push(ConfiguredTrackDefinition::try_from_input(
            track.to_configured_input(config.exchange.venue()),
        )?);
    }
    PreparedTrackRegistry::new(configured)
}

fn ensure_parent_dir(path: &std::path::Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("database path `{}` has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create database directory `{}`", parent.display()))
}

async fn detect_persisted_state_mismatches(
    prepared_registry: &PreparedTrackRegistry,
    repository: &SqliteStorage,
) -> Result<Vec<PersistedStateMismatch>> {
    let configured_ids = prepared_registry
        .iter()
        .map(|track| track.track_id().as_str().to_string())
        .collect::<std::collections::BTreeSet<_>>();
    let persisted_snapshots = TrackQueryStore::list_track_snapshots(repository).await?;
    let mut persisted_by_id = std::collections::HashMap::new();
    for stored in persisted_snapshots {
        persisted_by_id.insert(
            stored.snapshot.track_id.as_str().to_string(),
            stored.snapshot,
        );
    }
    let persisted_presence = repository
        .list_persisted_track_presence()
        .await?
        .into_iter()
        .map(|track_id| track_id.as_str().to_string())
        .collect::<std::collections::BTreeSet<_>>();

    let mut mismatches = Vec::new();
    for track in prepared_registry.iter() {
        let track_id = track.track_id().as_str();
        match persisted_by_id.remove(track_id) {
            Some(snapshot) if snapshot.restore_revision != *track.restore_revision() => {
                mismatches.push(PersistedStateMismatch {
                    track_id: track_id.to_string(),
                    detail: PersistedStateMismatchDetail::RestoreRevisionMismatch {
                        expected_revision: track.restore_revision().clone(),
                        actual_revision: snapshot.restore_revision,
                    },
                });
            }
            Some(_) => {}
            None if persisted_presence.contains(track_id) => {
                mismatches.push(PersistedStateMismatch {
                    track_id: track_id.to_string(),
                    detail: PersistedStateMismatchDetail::PersistedTrackMissingRuntime,
                });
            }
            None => {}
        }
    }

    let orphaned_track_ids = persisted_presence
        .iter()
        .chain(persisted_by_id.keys())
        .filter(|track_id| !configured_ids.contains(*track_id))
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();

    for track_id in orphaned_track_ids {
        mismatches.push(PersistedStateMismatch {
            track_id,
            detail: PersistedStateMismatchDetail::PersistedTrackMissingFromConfig,
        });
    }

    Ok(mismatches)
}

async fn hydrate_query_ready_state(
    prepared_registry: &PreparedTrackRegistry,
    repository: &SqliteStorage,
) -> Result<()> {
    let persisted_snapshots = TrackQueryStore::list_track_snapshots(repository).await?;
    let mut persisted_by_id = persisted_snapshots
        .into_iter()
        .map(|stored| {
            (
                stored.snapshot.track_id.as_str().to_string(),
                stored.snapshot,
            )
        })
        .collect::<std::collections::HashMap<_, _>>();

    for track in prepared_registry.iter() {
        let persisted_snapshot = persisted_by_id.remove(track.track_id().as_str());
        let next_snapshot = TrackRuntime::prepare_bootstrap_snapshot(
            track.runtime_seed(),
            persisted_snapshot.as_ref(),
            track.post_restore_constraints(),
            Utc::now(),
        )?;
        if persisted_snapshot
            .as_ref()
            .is_some_and(|snapshot| *snapshot == next_snapshot)
        {
            continue;
        }

        TrackMutationStore::save_transition(
            repository,
            track.track_id().as_str(),
            &next_snapshot,
            &[],
            &[],
        )
        .await?;
    }

    Ok(())
}

#[cfg(test)]
fn prepared_restore_revision(
    prepared_registry: &PreparedTrackRegistry,
    track_id: &str,
) -> poise_engine::persisted_runtime::TrackRestoreRevision {
    prepared_registry
        .get(&TrackId::new(track_id))
        .expect("track should exist in prepared registry")
        .restore_revision()
        .clone()
}

fn backup_and_reset_state_db(db_path: &std::path::Path) -> Result<Option<StateBackup>> {
    let timestamp = Utc::now()
        .to_rfc3339_opts(SecondsFormat::Nanos, true)
        .replace([':', '-'], "")
        .replace("+0000", "Z");
    let backup_path = next_backup_path_for_timestamp(db_path, &timestamp)?;

    let mut moved_files = Vec::new();
    for suffix in ["", "-wal", "-shm"] {
        let live_path = state_file_path(db_path, suffix);
        if !live_path.exists() {
            continue;
        }
        let backup_file_path = state_file_path(&backup_path, suffix);
        if let Err(error) = fs::rename(&live_path, &backup_file_path) {
            let restore_result = StateBackup { moved_files }.restore();
            return match restore_result {
                Ok(()) => Err(error).with_context(|| {
                    format!(
                        "failed to back up sqlite state from `{}` to `{}`",
                        live_path.display(),
                        backup_file_path.display()
                    )
                }),
                Err(restore_error) => Err(anyhow!(
                    "failed to back up sqlite state from `{}` to `{}`; restore also failed: {restore_error}",
                    live_path.display(),
                    backup_file_path.display()
                )),
            };
        }
        moved_files.push(BackupFile {
            live_path,
            backup_path: backup_file_path,
        });
    }

    if moved_files.is_empty() {
        Ok(None)
    } else {
        Ok(Some(StateBackup { moved_files }))
    }
}

fn remove_state_files(db_path: &std::path::Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let file_path = state_file_path(db_path, suffix);
        if !file_path.exists() {
            continue;
        }
        fs::remove_file(&file_path).with_context(|| {
            format!(
                "failed to remove sqlite state file `{}`",
                file_path.display()
            )
        })?;
    }
    Ok(())
}

fn next_backup_path_for_timestamp(db_path: &std::path::Path, timestamp: &str) -> Result<PathBuf> {
    let file_name = db_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("invalid sqlite file name `{}`", db_path.display()))?;
    for attempt in 0.. {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let candidate =
            db_path.with_file_name(format!("{file_name}.rebuild-{timestamp}{suffix}.bak"));
        if !state_backup_exists(&candidate) {
            return Ok(candidate);
        }
    }
    unreachable!("backup path search should always find an available candidate")
}

fn state_backup_exists(backup_path: &std::path::Path) -> bool {
    ["", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| state_file_path(backup_path, suffix))
        .any(|path| path.exists())
}

fn state_file_path(base_path: &std::path::Path, suffix: &str) -> PathBuf {
    if suffix.is_empty() {
        base_path.to_path_buf()
    } else {
        PathBuf::from(format!("{}{}", base_path.display(), suffix))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use poise_application::PreparedTrackRegistry;
    use poise_application::TrackMutationStore;
    use poise_application::TrackQueryStore;
    use poise_engine::manager::TrackManager;
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_storage::sqlite::SqliteStorage;

    use super::{StateBootstrapMode, prepare_state_repository};
    use crate::assembly::SystemClock;
    use crate::config::{Config, ExchangeConfig, TrackDefinition};

    #[tokio::test]
    async fn prepare_state_repository_requires_explicit_bootstrap_mode() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Strict)
            .await
            .unwrap();
        let _ = prepared.repositories.mutation_store();
    }

    #[tokio::test]
    async fn prepare_state_repository_builds_prepared_track_registry_for_startup() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Strict)
            .await
            .unwrap();

        let registry: &PreparedTrackRegistry = prepared.registry();
        assert_eq!(registry.iter().count(), 1);
        assert!(registry.get(&TrackId::new("btc-core")).is_some());
    }

    #[tokio::test]
    async fn state_repositories_exposes_query_store_via_application_owner() {
        let repository = std::sync::Arc::new(SqliteStorage::in_memory().unwrap());
        let repositories = super::StateRepositories::from_sqlite_storage(repository);
        let query_store = repositories.query_store();

        let snapshots = TrackQueryStore::list_track_snapshots(query_store.as_ref())
            .await
            .unwrap();
        assert!(snapshots.is_empty());
    }

    #[tokio::test]
    async fn strict_mode_rejects_persisted_restore_revision_mismatch() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        persist_snapshot_with_lower_price(&config, &db_path, 80.0).await;

        let error = prepare_state_repository(&config, &db_path, StateBootstrapMode::Strict)
            .await
            .err()
            .unwrap();

        assert!(
            error
                .to_string()
                .contains("persisted state does not match current config")
        );
    }

    #[tokio::test]
    async fn rebuild_mode_recreates_repository_after_mismatch() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        persist_snapshot_with_lower_price(&config, &db_path, 80.0).await;
        std::fs::write(format!("{}-wal", db_path.display()), b"wal").unwrap();
        std::fs::write(format!("{}-shm", db_path.display()), b"shm").unwrap();

        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Rebuild)
            .await
            .unwrap();

        let loaded = prepared
            .repositories
            .mutation_store()
            .load_track_state("btc-core")
            .await
            .unwrap();
        assert!(loaded.is_some());
        assert!(db_path.exists());
        assert!(!std::path::PathBuf::from(format!("{}-wal", db_path.display())).exists());
        assert!(!std::path::PathBuf::from(format!("{}-shm", db_path.display())).exists());
        let backup_exists = std::fs::read_dir(db_path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| {
                        name.starts_with("poise-server.sqlite.rebuild-") && name.ends_with(".bak")
                    })
                    .unwrap_or(false)
            });
        assert!(backup_exists);
        let backup_wal_exists = std::fs::read_dir(db_path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| {
                        name.starts_with("poise-server.sqlite.rebuild-")
                            && name.ends_with(".bak-wal")
                    })
                    .unwrap_or(false)
            });
        assert!(backup_wal_exists);
        let backup_shm_exists = std::fs::read_dir(db_path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| {
                        name.starts_with("poise-server.sqlite.rebuild-")
                            && name.ends_with(".bak-shm")
                    })
                    .unwrap_or(false)
            });
        assert!(backup_shm_exists);
    }

    #[tokio::test]
    async fn rebuild_mode_recovers_from_unreadable_existing_database() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        super::ensure_parent_dir(&db_path).unwrap();
        std::fs::write(&db_path, b"not-a-sqlite-database").unwrap();

        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Rebuild)
            .await
            .unwrap();

        let loaded = prepared
            .repositories
            .mutation_store()
            .load_track_state("btc-core")
            .await
            .unwrap();
        assert!(loaded.is_some());
        assert!(db_path.exists());
        let backup_exists = std::fs::read_dir(db_path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .map(|name| {
                        name.starts_with("poise-server.sqlite.rebuild-") && name.ends_with(".bak")
                    })
                    .unwrap_or(false)
            });
        assert!(backup_exists);
    }

    #[tokio::test]
    async fn restore_backup_recovers_original_state_after_rebuild() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        persist_snapshot_with_lower_price(&config, &db_path, 80.0).await;

        let mut prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Rebuild)
            .await
            .unwrap();
        prepared.restore_backup().unwrap();

        let restored = SqliteStorage::new(&db_path)
            .unwrap()
            .load_track_state("btc-core")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            restored.restore_revision,
            super::prepared_restore_revision(
                &super::build_prepared_registry(&test_config(80.0)).unwrap(),
                "btc-core",
            )
        );
    }

    #[tokio::test]
    async fn run_startup_restores_backup_after_failed_operation() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        persist_snapshot_with_lower_price(&config, &db_path, 80.0).await;

        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Rebuild)
            .await
            .unwrap();
        let error = prepared
            .run_startup(|_, _| async {
                Err::<(), anyhow::Error>(anyhow::anyhow!("startup failed"))
            })
            .await
            .unwrap_err();

        assert!(error.to_string().contains("startup failed"));
        let restored = SqliteStorage::new(&db_path)
            .unwrap()
            .load_track_state("btc-core")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            restored.restore_revision,
            super::prepared_restore_revision(
                &super::build_prepared_registry(&test_config(80.0)).unwrap(),
                "btc-core",
            )
        );
    }

    #[test]
    fn next_backup_path_appends_suffix_when_candidate_exists() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("poise-server.sqlite");
        let existing_backup = temp_dir
            .path()
            .join("poise-server.sqlite.rebuild-20260403T120000Z.bak");
        std::fs::write(&existing_backup, b"existing-backup").unwrap();

        let backup_path =
            super::next_backup_path_for_timestamp(&db_path, "20260403T120000Z").unwrap();

        assert_eq!(
            backup_path.file_name().and_then(|name| name.to_str()),
            Some("poise-server.sqlite.rebuild-20260403T120000Z-1.bak")
        );
    }

    #[tokio::test]
    async fn strict_mode_returns_structured_mismatch() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        persist_snapshot_with_lower_price(&config, &db_path, 80.0).await;

        let error = prepare_state_repository(&config, &db_path, StateBootstrapMode::Strict)
            .await
            .err()
            .unwrap();

        match error {
            super::StateBootstrapError::PersistedStateMismatch {
                db_path: actual_db_path,
                mismatches,
                suggested_action,
            } => {
                assert_eq!(actual_db_path, db_path);
                assert_eq!(mismatches.len(), 1);
                match &mismatches[0].detail {
                    super::PersistedStateMismatchDetail::RestoreRevisionMismatch {
                        expected_revision,
                        actual_revision,
                    } => {
                        assert_eq!(
                            expected_revision.as_str(),
                            super::prepared_restore_revision(
                                &super::build_prepared_registry(&config).unwrap(),
                                "btc-core",
                            )
                            .as_str()
                        );
                        assert_eq!(
                            actual_revision.as_str(),
                            super::prepared_restore_revision(
                                &super::build_prepared_registry(&test_config(80.0)).unwrap(),
                                "btc-core",
                            )
                            .as_str()
                        );
                    }
                    other => panic!("unexpected mismatch detail: {other:?}"),
                }
                assert_eq!(suggested_action, super::SuggestedAction::RebuildState);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn strict_mode_rejects_persisted_track_missing_from_config() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };
        let seeded_config = test_config(80.0);
        let db_path = test_db_path(instance_dir.path());
        persist_snapshot_with_lower_price(&seeded_config, &db_path, 80.0).await;

        let error = prepare_state_repository(&config, &db_path, StateBootstrapMode::Strict)
            .await
            .err()
            .unwrap();

        match error {
            super::StateBootstrapError::PersistedStateMismatch { mismatches, .. } => {
                assert_eq!(mismatches.len(), 1);
                match &mismatches[0].detail {
                    super::PersistedStateMismatchDetail::PersistedTrackMissingFromConfig => {
                        assert_eq!(mismatches[0].track_id, "btc-core");
                    }
                    other => panic!("unexpected mismatch detail: {other:?}"),
                }
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn strict_mode_seeds_initial_runtime_for_new_track_without_presence() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());

        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Strict)
            .await
            .unwrap();

        let loaded = prepared
            .repositories
            .mutation_store()
            .load_track_state("btc-core")
            .await
            .unwrap();
        assert!(loaded.is_some());
    }

    #[tokio::test]
    async fn strict_mode_rejects_persisted_track_missing_runtime_when_presence_exists() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        persist_snapshot_with_lower_price(&config, &db_path, 90.0).await;
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "DELETE FROM track_snapshots WHERE track_id = 'btc-core'",
            [],
        )
        .unwrap();

        let error = prepare_state_repository(&config, &db_path, StateBootstrapMode::Strict)
            .await
            .err()
            .unwrap();

        match error {
            super::StateBootstrapError::PersistedStateMismatch { mismatches, .. } => {
                assert_eq!(mismatches.len(), 1);
                assert_eq!(mismatches[0].track_id, "btc-core");
                assert!(matches!(
                    mismatches[0].detail,
                    super::PersistedStateMismatchDetail::PersistedTrackMissingRuntime
                ));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn strict_mode_applies_post_restore_constraints_without_restore_mismatch() {
        let instance_dir = tempfile::tempdir().unwrap();
        let mut config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        persist_snapshot_with_lower_price(&config, &db_path, 90.0).await;

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE track_snapshots
             SET ledger_state_json = ?1
             WHERE track_id = 'btc-core'",
            [serde_json::json!({
                "ledger_utc_day": "2026-04-23",
                "gross_realized_pnl_today": -150.0,
                "gross_realized_pnl_cumulative": -150.0,
                "trading_fee_today": 0.0,
                "trading_fee_cumulative": 0.0,
                "funding_fee_today": 0.0,
                "funding_fee_cumulative": 0.0,
                "unresolved_gaps": []
            })
            .to_string()],
        )
        .unwrap();
        config.tracks[0].total_loss_limit = 100.0;

        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Strict)
            .await
            .unwrap();
        let loaded = prepared
            .repositories
            .mutation_store()
            .load_track_state("btc-core")
            .await
            .unwrap()
            .unwrap();

        assert_eq!(
            loaded.desired_exposure,
            Some(poise_core::types::Exposure(0.0))
        );
    }

    #[tokio::test]
    async fn rebuild_mode_only_touches_database_under_current_instance_dir() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());

        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Rebuild)
            .await
            .unwrap();

        let loaded = prepared
            .repositories
            .mutation_store()
            .load_track_state("btc-core")
            .await
            .unwrap();
        assert!(loaded.is_some());
        assert!(db_path.exists());
        assert!(db_path.starts_with(instance_dir.path()));
    }

    #[test]
    fn state_bootstrap_tests_do_not_use_environment_bucket_helpers() {
        let source = include_str!("state_bootstrap.rs");
        let unique_test_environment_signature = ["fn ", "unique_test_environment", "()"].concat();
        let cleanup_environment_signature = ["fn ", "cleanup_environment", "("].concat();
        let test_db_path_signature = ["fn ", "test_db_path", "(environment:"].concat();
        let test_config_with_instance_dir_signature =
            ["fn ", "test_config_with_instance_dir", "("].concat();

        assert!(!source.contains(&unique_test_environment_signature));
        assert!(!source.contains(&cleanup_environment_signature));
        assert!(!source.contains(&test_db_path_signature));
        assert!(!source.contains(&test_config_with_instance_dir_signature));
    }

    fn test_config(lower_price: f64) -> Config {
        Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                symbol: "BTCUSDT".into(),
                lower_price,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: Some(0.5),
                shape_family: Some(poise_core::strategy::ShapeFamily::Linear),
                out_of_band_policy: Some(poise_core::strategy::BandProtectionPolicy::Freeze),
                max_notional: None,
                leverage: None,
                daily_loss_limit: 300.0,
                total_loss_limit: 600.0,
                tick_timeout_secs: None,
            }],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        }
    }

    fn test_db_path(instance_dir: &Path) -> PathBuf {
        crate::instance_dir::InstanceDir::new(instance_dir).db_path()
    }

    async fn persist_snapshot_with_lower_price(_config: &Config, db_path: &Path, lower_price: f64) {
        super::ensure_parent_dir(db_path).unwrap();
        let storage = SqliteStorage::new(db_path).unwrap();
        let mut manager = TrackManager::new(std::sync::Arc::new(SystemClock));
        manager
            .add_track(
                TrackId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
                poise_core::strategy::TrackConfig {
                    lower_price,
                    upper_price: 110.0,
                    long_exposure_units: 8.0,
                    short_exposure_units: 8.0,
                    notional_per_unit: 375.0,
                    min_rebalance_units: 0.5,
                    shape_family: poise_core::strategy::ShapeFamily::Linear,
                    out_of_band_policy: poise_core::strategy::BandProtectionPolicy::Freeze,
                },
                3000.0,
                poise_core::risk::LossLimits {
                    daily_loss_limit: 300.0,
                    total_loss_limit: 600.0,
                },
                poise_core::types::ExchangeRules {
                    price_tick: 0.1,
                    quantity_step: 0.1,
                    min_qty: 0.0,
                    min_notional: 0.0,
                    maker_fee_rate: 0.0,
                    taker_fee_rate: 0.0,
                },
            )
            .unwrap();
        storage
            .save_transition("btc-core", &manager.snapshot("btc-core").unwrap(), &[], &[])
            .await
            .unwrap();
    }
}
