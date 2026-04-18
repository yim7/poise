use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::error::{CommandError, CommandErrorKind};

#[derive(Debug, Clone)]
pub struct SessionStore {
    root_dir: PathBuf,
}

impl SessionStore {
    pub fn new(root_dir: impl AsRef<Path>) -> Self {
        Self {
            root_dir: root_dir.as_ref().to_path_buf(),
        }
    }

    pub fn load_json<T>(&self, config_path: impl AsRef<Path>) -> Result<Option<T>, CommandError>
    where
        T: DeserializeOwned,
    {
        let session_file = self.session_file_path(config_path.as_ref())?;
        if !session_file.exists() {
            return Ok(None);
        }

        let raw = fs::read_to_string(&session_file).map_err(|error| {
            CommandError::new(
                CommandErrorKind::SessionStore,
                format!("读取草稿会话失败 `{}`: {error}", session_file.display()),
            )
        })?;
        serde_json::from_str(&raw).map(Some).map_err(|error| {
            CommandError::new(
                CommandErrorKind::SessionStore,
                format!("解析草稿会话失败 `{}`: {error}", session_file.display()),
            )
        })
    }

    pub fn save_json<T>(&self, config_path: impl AsRef<Path>, value: &T) -> Result<(), CommandError>
    where
        T: Serialize,
    {
        fs::create_dir_all(&self.root_dir).map_err(|error| {
            CommandError::new(
                CommandErrorKind::SessionStore,
                format!("创建草稿目录失败 `{}`: {error}", self.root_dir.display()),
            )
        })?;

        let session_file = self.session_file_path(config_path.as_ref())?;
        let temp_file = session_file.with_extension("json.tmp");
        let serialized = serde_json::to_vec_pretty(value).map_err(|error| {
            CommandError::new(
                CommandErrorKind::SessionStore,
                format!("序列化草稿会话失败: {error}"),
            )
        })?;

        fs::write(&temp_file, serialized).map_err(|error| {
            CommandError::new(
                CommandErrorKind::SessionStore,
                format!("写入草稿会话失败 `{}`: {error}", temp_file.display()),
            )
        })?;
        fs::rename(&temp_file, &session_file).map_err(|error| {
            CommandError::new(
                CommandErrorKind::SessionStore,
                format!(
                    "落盘草稿会话失败 `{}` -> `{}`: {error}",
                    temp_file.display(),
                    session_file.display()
                ),
            )
        })?;
        Ok(())
    }

    pub fn session_key_for_path(config_path: impl AsRef<Path>) -> Result<String, CommandError> {
        let absolute_path = normalize_absolute_path(config_path.as_ref())?;
        let mut hasher = Sha256::new();
        hasher.update(absolute_path.to_string_lossy().as_bytes());
        Ok(format!("{:x}", hasher.finalize()))
    }

    fn session_file_path(&self, config_path: &Path) -> Result<PathBuf, CommandError> {
        Ok(self
            .root_dir
            .join(format!("{}.json", Self::session_key_for_path(config_path)?)))
    }
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf, CommandError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| {
                CommandError::new(
                    CommandErrorKind::Internal,
                    format!("获取当前目录失败: {error}"),
                )
            })?
            .join(path)
    };

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    Ok(normalized)
}
