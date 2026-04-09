use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{SecondsFormat, Utc};
use poise_application::{
    AccountMonitorStore, TrackEffectStore, TrackMutationStore, TrackQueryStore,
};
use poise_core::strategy::TrackConfig;
use poise_engine::track::Instrument;
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
    DefinitionChanged {
        expected_instrument: Instrument,
        actual_instrument: Instrument,
        expected_config: TrackConfig,
        actual_config: TrackConfig,
    },
    PersistedTrackMissingFromConfig {
        actual_instrument: Instrument,
        actual_config: TrackConfig,
    },
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
        F: FnOnce(StateRepositories) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        match startup(self.repositories.clone()).await {
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
    pub async fn load_track_state(
        &self,
        track_id: &str,
    ) -> Result<Option<poise_engine::snapshot::TrackRuntimeSnapshot>> {
        self.mutation_store.load_track_state(track_id).await
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
    ensure_parent_dir(&db_path).map_err(unexpected)?;
    let db_path = db_path.to_path_buf();

    match mode {
        StateBootstrapMode::Strict => {
            let repository = SqliteStorage::new(&db_path).map_err(unexpected)?;
            let mismatches = detect_persisted_state_mismatches(config, &repository)
                .await
                .map_err(unexpected)?;
            if mismatches.is_empty() {
                let repository = Arc::new(repository);
                return Ok(PreparedStateStore {
                    repositories: StateRepositories::from_sqlite_storage(repository),
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
            let repository = Arc::new(repository);
            Ok(PreparedStateStore {
                repositories: StateRepositories::from_sqlite_storage(repository),
                db_path,
                rebuild_backup,
            })
        }
    }
}

fn ensure_parent_dir(path: &std::path::Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("database path `{}` has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create database directory `{}`", parent.display()))
}

async fn detect_persisted_state_mismatches(
    config: &Config,
    repository: &SqliteStorage,
) -> Result<Vec<PersistedStateMismatch>> {
    let persisted_snapshots = TrackQueryStore::list_track_snapshots(repository).await?;
    let mut persisted_by_id = std::collections::HashMap::new();
    for stored in persisted_snapshots {
        persisted_by_id.insert(
            stored.snapshot.track_id.as_str().to_string(),
            stored.snapshot,
        );
    }

    let mut mismatches = Vec::new();
    for track in &config.tracks {
        let Some(snapshot) = persisted_by_id.remove(track.track_id.as_str()) else {
            continue;
        };

        let expected_instrument = track.instrument(config.exchange.venue());
        let actual_instrument = snapshot.instrument.clone();
        let expected_config = track.track_config();
        let actual_config = snapshot.config.clone();
        if expected_instrument != actual_instrument || expected_config != actual_config {
            mismatches.push(PersistedStateMismatch {
                track_id: track.track_id.clone(),
                detail: PersistedStateMismatchDetail::DefinitionChanged {
                    expected_instrument,
                    actual_instrument,
                    expected_config,
                    actual_config,
                },
            });
        }
    }

    for (track_id, snapshot) in persisted_by_id {
        mismatches.push(PersistedStateMismatch {
            track_id,
            detail: PersistedStateMismatchDetail::PersistedTrackMissingFromConfig {
                actual_instrument: snapshot.instrument,
                actual_config: snapshot.config,
            },
        });
    }

    Ok(mismatches)
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        let config = test_config(unique_test_environment(), 90.0);

        let db_path = crate::instance_dir::InstanceDir::new(std::env::temp_dir())
            .db_path(&config.environment);
        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Strict)
            .await
            .unwrap();
        let _ = prepared.repositories.mutation_store();
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
    async fn strict_mode_rejects_persisted_config_mismatch() {
        let environment = unique_test_environment();
        let config = test_config(environment.clone(), 90.0);
        let db_path = test_db_path(&environment);
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
        cleanup_environment(&environment);
    }

    #[tokio::test]
    async fn rebuild_mode_recreates_repository_after_mismatch() {
        let environment = unique_test_environment();
        let config = test_config(environment.clone(), 90.0);
        let db_path = test_db_path(&environment);
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
        assert!(loaded.is_none());
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
        cleanup_environment(&environment);
    }

    #[tokio::test]
    async fn rebuild_mode_recovers_from_unreadable_existing_database() {
        let environment = unique_test_environment();
        let config = test_config(environment.clone(), 90.0);
        let db_path = test_db_path(&environment);
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
        assert!(loaded.is_none());
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
        cleanup_environment(&environment);
    }

    #[tokio::test]
    async fn restore_backup_recovers_original_state_after_rebuild() {
        let environment = unique_test_environment();
        let config = test_config(environment.clone(), 90.0);
        let db_path = test_db_path(&environment);
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
        assert_eq!(restored.config.lower_price, 80.0);
        cleanup_environment(&environment);
    }

    #[tokio::test]
    async fn run_startup_restores_backup_after_failed_operation() {
        let environment = unique_test_environment();
        let config = test_config(environment.clone(), 90.0);
        let db_path = test_db_path(&environment);
        persist_snapshot_with_lower_price(&config, &db_path, 80.0).await;

        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Rebuild)
            .await
            .unwrap();
        let error = prepared
            .run_startup(|_| async { Err::<(), anyhow::Error>(anyhow::anyhow!("startup failed")) })
            .await
            .unwrap_err();

        assert!(error.to_string().contains("startup failed"));
        let restored = SqliteStorage::new(&db_path)
            .unwrap()
            .load_track_state("btc-core")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.config.lower_price, 80.0);
        cleanup_environment(&environment);
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
        let environment = unique_test_environment();
        let config = test_config(environment.clone(), 90.0);
        let db_path = test_db_path(&environment);
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
                    super::PersistedStateMismatchDetail::DefinitionChanged {
                        expected_config,
                        actual_config,
                        ..
                    } => {
                        assert_eq!(expected_config.lower_price, 90.0);
                        assert_eq!(actual_config.lower_price, 80.0);
                    }
                    other => panic!("unexpected mismatch detail: {other:?}"),
                }
                assert_eq!(suggested_action, super::SuggestedAction::RebuildState);
            }
            other => panic!("unexpected error: {other:?}"),
        }

        cleanup_environment(&environment);
    }

    #[tokio::test]
    async fn strict_mode_rejects_persisted_track_missing_from_config() {
        let environment = unique_test_environment();
        let config = Config {
            environment: environment.clone(),
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };
        let seeded_config = test_config(environment.clone(), 80.0);
        let db_path = test_db_path(&environment);
        persist_snapshot_with_lower_price(&seeded_config, &db_path, 80.0).await;

        let error = prepare_state_repository(&config, &db_path, StateBootstrapMode::Strict)
            .await
            .err()
            .unwrap();

        match error {
            super::StateBootstrapError::PersistedStateMismatch { mismatches, .. } => {
                assert_eq!(mismatches.len(), 1);
                match &mismatches[0].detail {
                    super::PersistedStateMismatchDetail::PersistedTrackMissingFromConfig {
                        actual_config,
                        ..
                    } => {
                        assert_eq!(mismatches[0].track_id, "btc-core");
                        assert_eq!(actual_config.lower_price, 80.0);
                    }
                    other => panic!("unexpected mismatch detail: {other:?}"),
                }
            }
            other => panic!("unexpected error: {other:?}"),
        }

        cleanup_environment(&environment);
    }

    #[tokio::test]
    async fn rebuild_mode_only_touches_database_under_current_instance_dir() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = test_config_with_instance_dir(temp_dir.path(), "mainnet", 90.0);
        let db_path = crate::instance_dir::InstanceDir::new(temp_dir.path()).db_path("mainnet");

        let prepared = prepare_state_repository(&config, &db_path, StateBootstrapMode::Rebuild)
            .await
            .unwrap();

        let loaded = prepared
            .repositories
            .mutation_store()
            .load_track_state("btc-core")
            .await
            .unwrap();
        assert!(loaded.is_none());
        assert!(db_path.exists());
    }

    fn unique_test_environment() -> String {
        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);
        format!(
            "state-bootstrap-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn test_config(environment: String, lower_price: f64) -> Config {
        Config {
            environment,
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![TrackDefinition {
                track_id: "btc-core".into(),
                symbol: "BTCUSDT".into(),
                lower_price,
                upper_price: 110.0,
                long_exposure_units: 8.0,
                short_exposure_units: 8.0,
                notional_per_unit: 375.0,
                min_rebalance_units: 0.5,
                shape_family: poise_core::strategy::ShapeFamily::Linear,
                out_of_band_policy: poise_core::strategy::OutOfBandPolicy::Freeze,
                max_notional: None,
                daily_loss_limit: None,
                stop_loss_pct: None,
                tick_timeout_secs: None,
            }],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        }
    }

    fn test_config_with_instance_dir(
        _instance_dir: &Path,
        environment: &str,
        lower_price: f64,
    ) -> Config {
        test_config(environment.to_string(), lower_price)
    }

    fn test_db_path(environment: &str) -> PathBuf {
        crate::instance_dir::InstanceDir::new(std::env::temp_dir()).db_path(environment)
    }

    async fn persist_snapshot_with_lower_price(_config: &Config, db_path: &Path, lower_price: f64) {
        super::ensure_parent_dir(&db_path).unwrap();
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
                    out_of_band_policy: poise_core::strategy::OutOfBandPolicy::Freeze,
                },
                poise_core::risk::CapacityBudget {
                    max_notional: 3000.0,
                    daily_loss_limit: -300.0,
                    stop_loss_pct: 10.0,
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

    fn cleanup_environment(environment: &str) {
        let _ = std::fs::remove_file(test_db_path(environment));
        let _ = std::fs::remove_file(format!("{}-wal", test_db_path(environment).display()));
        let _ = std::fs::remove_file(format!("{}-shm", test_db_path(environment).display()));
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(".data").join(environment));
    }
}
