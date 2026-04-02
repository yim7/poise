use std::fs;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use chrono::{SecondsFormat, Utc};
use poise_engine::ports::{StateRepositoryPort, StateStore};
use poise_engine::track::Instrument;
use poise_storage::sqlite::SqliteStorage;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateBootstrapMode {
    Strict,
    Rebuild,
}

#[derive(Debug, Clone, PartialEq)]
struct PersistedStateMismatch {
    track_id: String,
    expected_instrument: Instrument,
    actual_instrument: Instrument,
    expected_config_json: String,
    actual_config_json: String,
}

pub async fn prepare_state_repository(
    config: &Config,
    mode: StateBootstrapMode,
) -> Result<Arc<dyn StateStore>> {
    let db_path = config.default_db_path();
    ensure_parent_dir(&db_path)?;
    let repository = SqliteStorage::new(&db_path)?;
    let mismatches = detect_persisted_state_mismatches(config, &repository).await?;
    if mismatches.is_empty() {
        return Ok(Arc::new(repository));
    }

    match mode {
        StateBootstrapMode::Strict => Err(anyhow!(format_state_mismatch_error(
            &db_path,
            &mismatches
        ))),
        StateBootstrapMode::Rebuild => {
            drop(repository);
            backup_and_reset_state_db(&db_path)?;
            Ok(Arc::new(SqliteStorage::new(&db_path)?))
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
    let mut mismatches = Vec::new();
    for track in &config.tracks {
        let Some(snapshot) = repository.load_track_state(track.track_id.as_str()).await? else {
            continue;
        };

        let expected_instrument = track.instrument();
        let actual_instrument = snapshot.instrument.clone();
        let expected_config = track.track_config();
        let actual_config = snapshot.config.clone();
        if expected_instrument != actual_instrument || expected_config != actual_config {
            mismatches.push(PersistedStateMismatch {
                track_id: track.track_id.clone(),
                expected_instrument,
                actual_instrument,
                expected_config_json: serde_json::to_string(&expected_config)
                    .context("failed to serialize expected track config")?,
                actual_config_json: serde_json::to_string(&actual_config)
                    .context("failed to serialize persisted track config")?,
            });
        }
    }

    Ok(mismatches)
}

fn backup_and_reset_state_db(db_path: &std::path::Path) -> Result<()> {
    let timestamp = Utc::now()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
        .replace([':', '-'], "")
        .replace("+0000", "Z");
    let backup_path = db_path.with_file_name(format!(
        "{}.rebuild-{}.bak",
        db_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("invalid sqlite file name `{}`", db_path.display()))?,
        timestamp
    ));

    if db_path.exists() {
        fs::rename(db_path, &backup_path).with_context(|| {
            format!(
                "failed to back up sqlite database from `{}` to `{}`",
                db_path.display(),
                backup_path.display()
            )
        })?;
    }

    for suffix in ["-wal", "-shm"] {
        let sidecar_path = std::path::PathBuf::from(format!("{}{}", db_path.display(), suffix));
        if sidecar_path.exists() {
            fs::remove_file(&sidecar_path).with_context(|| {
                format!("failed to remove sqlite sidecar `{}`", sidecar_path.display())
            })?;
        }
    }

    Ok(())
}

fn format_state_mismatch_error(
    db_path: &std::path::Path,
    mismatches: &[PersistedStateMismatch],
) -> String {
    let mut message = format!(
        "persisted state does not match current config in `{}`.\n\
use `--rebuild-state` to back up the old database, discard local snapshots, and rebuild state from the exchange's live positions and orders.\n\
suggested command: cargo run -p poise-server -- --config <path> --rebuild-state",
        db_path.display()
    );

    for mismatch in mismatches {
        let instrument_line = if mismatch.expected_instrument != mismatch.actual_instrument {
            format!(
                "\n  instrument: expected `{}:{}`, persisted `{}:{}`",
                mismatch.expected_instrument.venue.as_str(),
                mismatch.expected_instrument.symbol,
                mismatch.actual_instrument.venue.as_str(),
                mismatch.actual_instrument.symbol
            )
        } else {
            String::new()
        };

        message.push_str(&format!(
            "\ntrack `{}`:{}\n  expected config: {}\n  persisted config: {}",
            mismatch.track_id,
            instrument_line,
            mismatch.expected_config_json,
            mismatch.actual_config_json
        ));
    }

    message
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use poise_engine::manager::TrackManager;
    use poise_engine::ports::StateRepositoryPort;
    use poise_engine::track::{Instrument, TrackId, Venue};
    use poise_storage::sqlite::SqliteStorage;

    use super::{StateBootstrapMode, prepare_state_repository};
    use crate::config::{Config, ExchangeConfig, TrackDefinition};
    use crate::assembly::SystemClock;

    #[tokio::test]
    async fn prepare_state_repository_requires_explicit_bootstrap_mode() {
        let config = test_config(unique_test_environment(), 90.0);

        let repository = prepare_state_repository(&config, StateBootstrapMode::Strict)
            .await
            .unwrap();
        let _ = repository.into_state_repository();
    }

    #[tokio::test]
    async fn strict_mode_rejects_persisted_config_mismatch() {
        let environment = unique_test_environment();
        let config = test_config(environment.clone(), 90.0);
        persist_snapshot_with_lower_price(&config, 80.0).await;

        let error = prepare_state_repository(&config, StateBootstrapMode::Strict)
            .await
            .err()
            .unwrap();

        assert!(error.to_string().contains("persisted state does not match current config"));
        cleanup_environment(&environment);
    }

    #[tokio::test]
    async fn rebuild_mode_recreates_repository_after_mismatch() {
        let environment = unique_test_environment();
        let config = test_config(environment.clone(), 90.0);
        let db_path = config.default_db_path();
        persist_snapshot_with_lower_price(&config, 80.0).await;
        std::fs::write(format!("{}-wal", db_path.display()), b"wal").unwrap();
        std::fs::write(format!("{}-shm", db_path.display()), b"shm").unwrap();

        let repository = prepare_state_repository(&config, StateBootstrapMode::Rebuild)
            .await
            .unwrap();

        let loaded = repository
            .into_state_repository()
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
        cleanup_environment(&environment);
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
                venue: Venue::Binance,
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
        }
    }

    async fn persist_snapshot_with_lower_price(config: &Config, lower_price: f64) {
        let db_path = config.default_db_path();
        super::ensure_parent_dir(&db_path).unwrap();
        let storage = SqliteStorage::new(&db_path).unwrap();
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
        let _ = std::fs::remove_dir_all(std::path::Path::new(".data").join(environment));
    }
}
