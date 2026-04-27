use std::{
    fs,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
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
        let temp_file = temporary_session_file_path(&session_file);
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
        replace_file_preserving_existing(&temp_file, &session_file).inspect_err(|_error| {
            let _ = fs::remove_file(&temp_file);
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

fn temporary_session_file_path(session_file: &Path) -> PathBuf {
    let unique_suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = session_file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.json");
    session_file.with_file_name(format!("{file_name}.{unique_suffix}.tmp"))
}

fn backup_session_file_path(session_file: &Path) -> PathBuf {
    let unique_suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = session_file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session.json");
    session_file.with_file_name(format!("{file_name}.{unique_suffix}.bak"))
}

fn replace_file_preserving_existing(
    temp_file: &Path,
    session_file: &Path,
) -> Result<(), CommandError> {
    replace_file_preserving_existing_with(
        temp_file,
        session_file,
        session_file.exists(),
        &backup_session_file_path(session_file),
        |from, to| fs::rename(from, to),
        |path| fs::remove_file(path),
    )
}

fn replace_file_preserving_existing_with<Rename, Remove>(
    temp_file: &Path,
    session_file: &Path,
    had_existing_file: bool,
    backup_file: &Path,
    rename: Rename,
    remove_file: Remove,
) -> Result<(), CommandError>
where
    Rename: Fn(&Path, &Path) -> std::io::Result<()>,
    Remove: Fn(&Path) -> std::io::Result<()>,
{
    if had_existing_file {
        rename(session_file, backup_file).map_err(|error| {
            CommandError::new(
                CommandErrorKind::SessionStore,
                format!(
                    "为草稿会话创建备份失败 `{}` -> `{}`: {error}",
                    session_file.display(),
                    backup_file.display()
                ),
            )
        })?;
    }

    match rename(temp_file, session_file) {
        Ok(()) => {
            if had_existing_file {
                remove_file(backup_file).map_err(|error| {
                    CommandError::new(
                        CommandErrorKind::SessionStore,
                        format!("清理草稿备份失败 `{}`: {error}", backup_file.display()),
                    )
                })?;
            }
            Ok(())
        }
        Err(error) => {
            if had_existing_file && let Err(restore_error) = rename(backup_file, session_file) {
                return Err(CommandError::new(
                    CommandErrorKind::SessionStore,
                    format!(
                        "落盘草稿会话失败 `{}` -> `{}`: {error}; 恢复旧草稿失败 `{}` -> `{}`: {restore_error}",
                        temp_file.display(),
                        session_file.display(),
                        backup_file.display(),
                        session_file.display()
                    ),
                ));
            }
            Err(CommandError::new(
                CommandErrorKind::SessionStore,
                format!(
                    "落盘草稿会话失败 `{}` -> `{}`: {error}",
                    temp_file.display(),
                    session_file.display()
                ),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashSet, path::PathBuf};

    use super::replace_file_preserving_existing_with;

    #[test]
    fn replace_failure_restores_existing_file() {
        let temp_file = PathBuf::from("/tmp/new.json.tmp");
        let session_file = PathBuf::from("/tmp/session.json");
        let backup_file = PathBuf::from("/tmp/session.json.bak");
        let files = RefCell::new(HashSet::from([temp_file.clone(), session_file.clone()]));

        let error = replace_file_preserving_existing_with(
            &temp_file,
            &session_file,
            true,
            &backup_file,
            |from, to| {
                if from == temp_file.as_path() && to == session_file.as_path() {
                    return Err(std::io::Error::other("simulated rename failure"));
                }
                let mut files = files.borrow_mut();
                if !files.remove(from) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "source missing",
                    ));
                }
                files.insert(to.to_path_buf());
                Ok(())
            },
            |path| {
                let mut files = files.borrow_mut();
                files.remove(path);
                Ok(())
            },
        )
        .unwrap_err();

        let files = files.into_inner();
        assert!(files.contains(&session_file));
        assert!(files.contains(&temp_file));
        assert!(!files.contains(&backup_file));
        assert!(error.message.contains("simulated rename failure"));
    }

    #[test]
    fn replace_success_removes_backup_after_swap() {
        let temp_file = PathBuf::from("/tmp/new.json.tmp");
        let session_file = PathBuf::from("/tmp/session.json");
        let backup_file = PathBuf::from("/tmp/session.json.bak");
        let files = RefCell::new(HashSet::from([temp_file.clone(), session_file.clone()]));
        let removed = RefCell::new(Vec::<PathBuf>::new());

        replace_file_preserving_existing_with(
            &temp_file,
            &session_file,
            true,
            &backup_file,
            |from, to| {
                let mut files = files.borrow_mut();
                if !files.remove(from) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "source missing",
                    ));
                }
                files.insert(to.to_path_buf());
                Ok(())
            },
            |path| {
                removed.borrow_mut().push(path.to_path_buf());
                let mut files = files.borrow_mut();
                files.remove(path);
                Ok(())
            },
        )
        .unwrap();

        let files = files.into_inner();
        assert!(files.contains(&session_file));
        assert!(!files.contains(&backup_file));
        assert_eq!(removed.into_inner(), vec![backup_file]);
    }
}
