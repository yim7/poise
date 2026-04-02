use std::sync::Arc;

use anyhow::{Context, Result};
use poise_engine::ports::StateStore;
use poise_storage::sqlite::SqliteStorage;

use crate::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateBootstrapMode {
    Strict,
    Rebuild,
}

pub async fn prepare_state_repository(
    config: &Config,
    _mode: StateBootstrapMode,
) -> Result<Arc<dyn StateStore>> {
    let db_path = config.default_db_path();
    ensure_parent_dir(&db_path)?;
    Ok(Arc::new(SqliteStorage::new(&db_path)?))
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
    use super::{StateBootstrapMode, prepare_state_repository};
    use crate::config::{Config, ExchangeConfig};

    #[tokio::test]
    async fn prepare_state_repository_requires_explicit_bootstrap_mode() {
        let config = Config {
            environment: "testnet".into(),
            bind_address: "127.0.0.1:0".into(),
            tracks: Vec::new(),
            exchange: ExchangeConfig::default(),
        };

        let repository = prepare_state_repository(&config, StateBootstrapMode::Strict)
            .await
            .unwrap();
        let _ = repository.into_state_repository();
    }
}
