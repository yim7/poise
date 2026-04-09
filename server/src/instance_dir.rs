use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceDir {
    root: PathBuf,
}

impl InstanceDir {
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            root: path.as_ref().to_path_buf(),
        }
    }

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn data_root(&self) -> PathBuf {
        self.root.join(".data")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_root().join("poise-server.sqlite")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::InstanceDir;

    #[test]
    fn instance_dir_module_does_not_expose_logs_root() {
        let source = include_str!("instance_dir.rs");
        let logs_root_signature = ["pub", " fn", " logs_root", "("].concat();

        assert!(!source.contains(&logs_root_signature));
    }

    #[test]
    fn instance_dir_resolves_config_and_data_paths() {
        let dir = InstanceDir::new("/tmp/poise/a");

        assert_eq!(dir.config_path(), PathBuf::from("/tmp/poise/a/config.toml"));
        assert_eq!(dir.data_root(), PathBuf::from("/tmp/poise/a/.data"));
    }

    #[test]
    fn instance_dir_db_path_is_fixed_under_instance_data_root() {
        let dir = InstanceDir::new("/tmp/poise/a");

        assert_eq!(
            dir.db_path(),
            PathBuf::from("/tmp/poise/a/.data/poise-server.sqlite")
        );
    }
}
