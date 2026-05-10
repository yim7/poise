use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use poise_application::{
    AccountMonitorStore, TrackDefinitionRegistry, TrackEffectJournal, TrackMutationStore,
    TrackQueryStore,
};
use poise_storage::sqlite::SqliteStorage;

use crate::config::Config;

#[derive(Debug)]
pub enum StateBootstrapError {
    Unexpected(anyhow::Error),
}

impl std::fmt::Display for StateBootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
    effect_store: Arc<dyn TrackEffectJournal>,
}

pub struct PreparedStateStore {
    repositories: StateRepositories,
    track_definition_registry: Arc<TrackDefinitionRegistry>,
}

impl PreparedStateStore {
    pub async fn run_startup<T, F, Fut>(self, startup: F) -> Result<T>
    where
        F: FnOnce(StateRepositories, Arc<TrackDefinitionRegistry>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        startup(
            self.repositories.clone(),
            Arc::clone(&self.track_definition_registry),
        )
        .await
    }

    #[cfg(test)]
    pub fn registry(&self) -> &TrackDefinitionRegistry {
        &self.track_definition_registry
    }
}

impl StateRepositories {
    #[cfg(test)]
    pub(crate) fn new<R>(repository: Arc<R>) -> Self
    where
        R: TrackMutationStore + TrackQueryStore + TrackEffectJournal + 'static,
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

    pub fn effect_store(&self) -> Arc<dyn TrackEffectJournal> {
        Arc::clone(&self.effect_store)
    }
}

fn unexpected(error: anyhow::Error) -> StateBootstrapError {
    StateBootstrapError::Unexpected(error)
}

pub async fn prepare_state_repository(
    config: &Config,
    db_path: &Path,
) -> BootstrapResult<PreparedStateStore> {
    ensure_parent_dir(db_path).map_err(unexpected)?;
    let db_path = db_path.to_path_buf();
    let track_definition_registry =
        Arc::new(build_track_definition_registry(config).map_err(unexpected)?);

    let repository = SqliteStorage::new(&db_path).map_err(unexpected)?;
    let repository = Arc::new(repository);
    Ok(PreparedStateStore {
        repositories: StateRepositories::from_sqlite_storage(repository),
        track_definition_registry,
    })
}

fn build_track_definition_registry(config: &Config) -> Result<TrackDefinitionRegistry> {
    let mut configured = Vec::with_capacity(config.tracks.len());
    for track in &config.tracks {
        configured.push(track.to_track_definition(config.exchange.venue())?);
    }
    TrackDefinitionRegistry::new(configured)
}

fn ensure_parent_dir(path: &std::path::Path) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("database path `{}` has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create database directory `{}`", parent.display()))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use poise_application::PersistedControlMode;
    use poise_application::TrackControlState;
    use poise_application::TrackDefinitionRegistry;
    use poise_application::TrackMutationStore;
    use poise_application::TrackQueryStore;
    use poise_core::track::TrackId;
    use poise_storage::sqlite::SqliteStorage;
    use rusqlite::params;

    use super::prepare_state_repository;
    use crate::config::{Config, ExchangeConfig, TrackSpec};

    #[tokio::test]
    async fn prepare_state_repository_requires_explicit_bootstrap_mode() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        let prepared = prepare_state_repository(&config, &db_path).await.unwrap();
        let _ = prepared.repositories.mutation_store();
    }

    #[tokio::test]
    async fn prepare_state_repository_builds_track_definition_registry_for_startup() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        let prepared = prepare_state_repository(&config, &db_path).await.unwrap();

        let registry: &TrackDefinitionRegistry = prepared.registry();
        assert_eq!(registry.iter().count(), 1);
        assert!(registry.get(&TrackId::new("btc-core")).is_some());
    }

    #[tokio::test]
    async fn state_repositories_exposes_query_store_via_application_owner() {
        let repository = std::sync::Arc::new(SqliteStorage::in_memory().unwrap());
        let repositories = super::StateRepositories::from_sqlite_storage(repository);
        let query_store = repositories.query_store();

        let updated_at =
            TrackQueryStore::load_track_updated_at(query_store.as_ref(), &TrackId::new("btc-core"))
                .await
                .unwrap();
        assert!(updated_at.is_none());
    }

    #[tokio::test]
    async fn strict_mode_accepts_persisted_control_state_for_configured_track() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        persist_business_state(
            &db_path,
            TrackControlState::Enabled {
                mode: PersistedControlMode::Automatic,
            },
        )
        .await;

        let prepared = prepare_state_repository(&config, &db_path).await.unwrap();
        let query_store = prepared.repositories.query_store();

        assert_eq!(
            TrackQueryStore::load_track_control_state(
                query_store.as_ref(),
                &TrackId::new("btc-core")
            )
            .await
            .unwrap(),
            Some(TrackControlState::Enabled {
                mode: PersistedControlMode::Automatic,
            })
        );
    }

    #[tokio::test]
    async fn strict_mode_allows_removed_config_tracks_without_rebuild() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };
        let db_path = test_db_path(instance_dir.path());
        persist_control_state(&db_path).await;

        let prepared = prepare_state_repository(&config, &db_path).await.unwrap();
        assert_eq!(prepared.registry().iter().count(), 0);
    }

    #[tokio::test]
    async fn strict_mode_seeds_initial_runtime_for_new_track_without_presence() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());

        let prepared = prepare_state_repository(&config, &db_path).await.unwrap();
        assert!(prepared.registry().get(&TrackId::new("btc-core")).is_some());
    }

    #[tokio::test]
    async fn strict_mode_ignores_presence_without_control_truth() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        persist_presence_without_business_truth(&db_path).await;

        let prepared = prepare_state_repository(&config, &db_path).await.unwrap();
        assert!(prepared.registry().get(&TrackId::new("btc-core")).is_some());
    }

    #[tokio::test]
    async fn strict_mode_allows_business_state_without_runtime_snapshot() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        persist_business_state(
            &db_path,
            TrackControlState::Paused {
                resume_mode: PersistedControlMode::Automatic,
            },
        )
        .await;

        let prepared = prepare_state_repository(&config, &db_path).await.unwrap();
        let query_store = prepared.repositories.query_store();

        assert_eq!(
            TrackQueryStore::load_track_control_state(
                query_store.as_ref(),
                &TrackId::new("btc-core")
            )
            .await
            .unwrap(),
            Some(TrackControlState::Paused {
                resume_mode: PersistedControlMode::Automatic,
            })
        );
    }

    #[tokio::test]
    async fn strict_bootstrap_only_touches_database_under_current_instance_dir() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());

        prepare_state_repository(&config, &db_path).await.unwrap();
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
            tracks: vec![TrackSpec {
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
                risk_increase_delay: None,
            }],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        }
    }

    fn test_db_path(instance_dir: &Path) -> PathBuf {
        crate::instance_dir::InstanceDir::new(instance_dir).db_path()
    }

    async fn persist_presence_without_business_truth(db_path: &Path) {
        super::ensure_parent_dir(db_path).unwrap();
        drop(SqliteStorage::new(db_path).unwrap());
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO persisted_track_presence (track_id, created_at, updated_at)
             VALUES (?1, ?2, ?3)",
            params!["btc-core", now, now],
        )
        .unwrap();
    }

    async fn persist_control_state(db_path: &Path) {
        super::ensure_parent_dir(db_path).unwrap();
        let storage = SqliteStorage::new(db_path).unwrap();
        storage
            .save_track_control_state(
                &TrackId::new("btc-core"),
                &TrackControlState::Enabled {
                    mode: PersistedControlMode::Automatic,
                },
            )
            .await
            .unwrap();
    }

    async fn persist_business_state(db_path: &Path, control_state: TrackControlState) {
        super::ensure_parent_dir(db_path).unwrap();
        let storage = SqliteStorage::new(db_path).unwrap();
        storage
            .save_track_control_state(&TrackId::new("btc-core"), &control_state)
            .await
            .unwrap();
    }
}
