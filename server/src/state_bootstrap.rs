use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use poise_application::{
    AccountMonitorStore, TrackDefinitionRegistry, TrackEffectJournal, TrackIdentityGuardOutcome,
    TrackIdentityStore, TrackMutationStore, TrackQueryStore,
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
    identity_store: Arc<dyn TrackIdentityStore>,
    track_definition_registry: Arc<TrackDefinitionRegistry>,
}

impl PreparedStateStore {
    pub async fn run_startup<T, F, Fut>(self, startup: F) -> Result<T>
    where
        F: FnOnce(StateRepositories, Arc<TrackDefinitionRegistry>) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let identities = self
            .track_definition_registry
            .iter()
            .map(|definition| {
                (
                    definition.track_id().clone(),
                    definition.instrument().clone(),
                )
            })
            .collect::<Vec<_>>();
        let outcomes = self
            .identity_store
            .guard_track_identities(&identities)
            .await
            .context("failed to guard configured track identities")?;
        log_track_identity_guard_outcomes(&self.track_definition_registry, &outcomes);

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

fn log_track_identity_guard_outcomes(
    registry: &TrackDefinitionRegistry,
    outcomes: &[TrackIdentityGuardOutcome],
) {
    for outcome in outcomes {
        match outcome {
            TrackIdentityGuardOutcome::AdoptedLegacyPnl(track_id) => {
                let definition = registry
                    .get(track_id)
                    .expect("identity guard outcome must reference a configured track");
                let instrument = definition.instrument();
                tracing::info!(
                    track_id = %track_id.as_str(),
                    venue = instrument.venue.as_str(),
                    symbol = %instrument.symbol,
                    adoption_source = "legacy_pnl",
                    "adopted configured track identity from legacy PNL evidence"
                );
            }
            TrackIdentityGuardOutcome::AdoptedLegacyWithoutInstrumentEvidence(track_id) => {
                let definition = registry
                    .get(track_id)
                    .expect("identity guard outcome must reference a configured track");
                let instrument = definition.instrument();
                tracing::warn!(
                    track_id = %track_id.as_str(),
                    venue = instrument.venue.as_str(),
                    symbol = %instrument.symbol,
                    adoption_source = "control_or_events_without_instrument",
                    "adopted configured track identity for legacy state without instrument evidence"
                );
            }
            TrackIdentityGuardOutcome::Matched(_) | TrackIdentityGuardOutcome::BoundNew(_) => {}
        }
    }
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
        repositories: StateRepositories::from_sqlite_storage(repository.clone()),
        identity_store: repository,
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use poise_application::PersistedControlMode;
    use poise_application::TrackControlState;
    use poise_application::TrackDefinitionRegistry;
    use poise_application::TrackIdentityStore;
    use poise_application::TrackMutationStore;
    use poise_application::TrackQueryStore;
    use poise_core::track::{Instrument, TrackId, Venue};
    use poise_storage::sqlite::SqliteStorage;
    use rusqlite::{OptionalExtension, params};

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
    async fn track_identity_is_bound_before_startup_and_matching_restart_keeps_bound_at() {
        let instance_dir = tempfile::tempdir().unwrap();
        let config = test_config(90.0);
        let db_path = test_db_path(instance_dir.path());
        let first_query_path = db_path.clone();
        let first = prepare_state_repository(&config, &db_path)
            .await
            .unwrap()
            .run_startup(move |_repositories, _registry| async move {
                Ok(load_track_identity(&first_query_path, "btc-core").unwrap())
            })
            .await
            .unwrap();
        let second_query_path = db_path.clone();
        let second = prepare_state_repository(&config, &db_path)
            .await
            .unwrap()
            .run_startup(move |_repositories, _registry| async move {
                Ok(load_track_identity(&second_query_path, "btc-core").unwrap())
            })
            .await
            .unwrap();

        assert_eq!(first.0, "binance");
        assert_eq!(first.1, "BTCUSDT");
        assert!(!first.2.is_empty());
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn track_identity_conflict_does_not_invoke_startup_closure() {
        let instance_dir = tempfile::tempdir().unwrap();
        let db_path = test_db_path(instance_dir.path());
        super::ensure_parent_dir(&db_path).unwrap();
        let storage = SqliteStorage::new(&db_path).unwrap();
        TrackIdentityStore::guard_track_identities(
            &storage,
            &[(
                TrackId::new("btc-core"),
                Instrument::new(Venue::Binance, "BTCUSDT"),
            )],
        )
        .await
        .unwrap();
        drop(storage);
        let mut config = test_config(90.0);
        config.tracks[0].symbol = "ETHUSDT".into();
        let closure_invoked = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&closure_invoked);

        let error = prepare_state_repository(&config, &db_path)
            .await
            .unwrap()
            .run_startup(move |_repositories, _registry| async move {
                observed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("track identity"));
        assert!(!closure_invoked.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn matching_legacy_pnl_identity_is_adopted_before_startup() {
        let instance_dir = tempfile::tempdir().unwrap();
        let db_path = test_db_path(instance_dir.path());
        super::ensure_parent_dir(&db_path).unwrap();
        drop(SqliteStorage::new(&db_path).unwrap());
        insert_legacy_pnl_identity(&db_path, "btc-core", "binance", "BTCUSDT");
        let config = test_config(90.0);

        prepare_state_repository(&config, &db_path)
            .await
            .unwrap()
            .run_startup(|_repositories, _registry| async { Ok(()) })
            .await
            .unwrap();

        let identity = load_track_identity(&db_path, "btc-core").unwrap();
        assert_eq!(
            (identity.0.as_str(), identity.1.as_str()),
            ("binance", "BTCUSDT")
        );
    }

    #[tokio::test]
    async fn conflicting_legacy_pnl_identity_does_not_invoke_startup() {
        let instance_dir = tempfile::tempdir().unwrap();
        let db_path = test_db_path(instance_dir.path());
        super::ensure_parent_dir(&db_path).unwrap();
        drop(SqliteStorage::new(&db_path).unwrap());
        insert_legacy_pnl_identity(&db_path, "btc-core", "binance", "BTCUSDT");
        let mut config = test_config(90.0);
        config.tracks[0].symbol = "ETHUSDT".into();
        let closure_invoked = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&closure_invoked);

        let error = prepare_state_repository(&config, &db_path)
            .await
            .unwrap()
            .run_startup(move |_repositories, _registry| async move {
                observed.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("legacy PNL identity"));
        assert!(!closure_invoked.load(Ordering::SeqCst));
        assert!(load_track_identity(&db_path, "btc-core").is_none());
    }

    #[tokio::test]
    async fn control_only_legacy_state_adopts_current_track_identity() {
        let instance_dir = tempfile::tempdir().unwrap();
        let db_path = test_db_path(instance_dir.path());
        persist_business_state(
            &db_path,
            TrackControlState::Paused {
                resume_mode: PersistedControlMode::Automatic,
            },
        )
        .await;
        let config = test_config(90.0);

        let state = prepare_state_repository(&config, &db_path)
            .await
            .unwrap()
            .run_startup(|repositories, _registry| async move {
                TrackQueryStore::load_track_control_state(
                    repositories.query_store().as_ref(),
                    &TrackId::new("btc-core"),
                )
                .await
            })
            .await
            .unwrap();

        assert_eq!(
            state,
            Some(TrackControlState::Paused {
                resume_mode: PersistedControlMode::Automatic,
            })
        );
        assert!(load_track_identity(&db_path, "btc-core").is_some());
    }

    #[tokio::test]
    async fn removed_track_keeps_identity_and_cannot_be_reused_for_another_symbol() {
        let instance_dir = tempfile::tempdir().unwrap();
        let db_path = test_db_path(instance_dir.path());
        let config = test_config(90.0);
        prepare_state_repository(&config, &db_path)
            .await
            .unwrap()
            .run_startup(|_repositories, _registry| async { Ok(()) })
            .await
            .unwrap();
        let empty_config = Config {
            bind_address: "127.0.0.1:0".into(),
            tracks: vec![],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        };
        prepare_state_repository(&empty_config, &db_path)
            .await
            .unwrap()
            .run_startup(|_repositories, _registry| async { Ok(()) })
            .await
            .unwrap();
        assert!(load_track_identity(&db_path, "btc-core").is_some());

        let mut reused_config = test_config(90.0);
        reused_config.tracks[0].symbol = "ETHUSDT".into();
        let error = prepare_state_repository(&reused_config, &db_path)
            .await
            .unwrap()
            .run_startup(|_repositories, _registry| async { Ok(()) })
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("track identity conflict"));
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
                risk_acquisition: Default::default(),
            }],
            exchange: ExchangeConfig::default(),
            account_monitor: Default::default(),
        }
    }

    fn test_db_path(instance_dir: &Path) -> PathBuf {
        crate::instance_dir::InstanceDir::new(instance_dir).db_path()
    }

    fn load_track_identity(db_path: &Path, track_id: &str) -> Option<(String, String, String)> {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT venue, symbol, bound_at
             FROM track_identity
             WHERE track_id = ?1",
            params![track_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .unwrap()
    }

    fn insert_legacy_pnl_identity(db_path: &Path, track_id: &str, venue: &str, symbol: &str) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute(
            "INSERT INTO track_pnl_records (
                track_id,
                venue,
                symbol,
                occurred_at,
                kind,
                pnl_asset,
                source,
                source_key
            ) VALUES (?1, ?2, ?3, ?4, 'trade', 'USDT', 'legacy:test', ?5)",
            params![
                track_id,
                venue,
                symbol,
                chrono::Utc::now().to_rfc3339(),
                format!("legacy:{track_id}:{venue}:{symbol}"),
            ],
        )
        .unwrap();
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
