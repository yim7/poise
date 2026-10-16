//! 实例锁：同一个数据目录同一时刻只允许一个进程写。
//!
//! 两个进程同时驱动同一个策略，会各自规划、各自下单：同一格被买两次，数据库里的
//! 订单记录也会互相覆盖。所以会写数据库、或者会向交易所下单撤单的命令，动手之前
//! 先在这个数据目录上取一把独占锁；取不到就拒绝运行，并说明是谁占着。
//!
//! 用的是进程级的文件锁（`flock`）：进程正常退出、崩溃或被杀掉，锁都由操作系统释放，
//! 不会留下需要手工清理的死锁文件。锁文件本身只是一块占位，没有内容。

use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};

/// 锁文件名，放在数据目录下。
pub const LOCK_FILE_NAME: &str = "poise.lock";

/// 持有的实例锁。析构（或进程结束）时释放。
#[derive(Debug)]
pub struct InstanceLock {
    /// 一直握着这个句柄：文件一关，锁就没了。
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// 在数据目录上取独占锁。目录必须已经存在。
    pub fn acquire(data_dir: impl AsRef<Path>) -> Result<Self, LockError> {
        let path = data_dir.as_ref().join(LOCK_FILE_NAME);
        let file = File::create(&path).map_err(|source| LockError::Io {
            path: path.clone(),
            source,
        })?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file, path }),
            Err(TryLockError::WouldBlock) => Err(LockError::AlreadyRunning { path }),
            Err(TryLockError::Error(source)) => Err(LockError::Io { path, source }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error(
        "another poise process is already running on this instance (it holds `{path}`); two processes on one instance would place duplicate orders, so this one refuses to start"
    )]
    AlreadyRunning { path: PathBuf },
    #[error("cannot create the lock file `{path}`: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::{InstanceLock, LOCK_FILE_NAME, LockError};

    #[test]
    fn a_second_lock_on_the_same_directory_is_refused() {
        let directory = tempfile::tempdir().unwrap();

        let first = InstanceLock::acquire(directory.path()).unwrap();
        let error = InstanceLock::acquire(directory.path()).unwrap_err();

        assert!(
            matches!(error, LockError::AlreadyRunning { .. }),
            "unexpected error: {error}"
        );
        // 说清楚是谁占着，用户才知道去哪里找那个进程。
        assert!(
            error.to_string().contains(LOCK_FILE_NAME),
            "unexpected message: {error}"
        );
        assert_eq!(first.path(), directory.path().join(LOCK_FILE_NAME));
    }

    #[test]
    fn the_lock_is_released_when_it_is_dropped() {
        let directory = tempfile::tempdir().unwrap();

        drop(InstanceLock::acquire(directory.path()).unwrap());

        assert!(InstanceLock::acquire(directory.path()).is_ok());
    }

    #[test]
    fn a_missing_directory_is_an_io_error_not_a_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("data");

        let error = InstanceLock::acquire(&missing).unwrap_err();

        assert!(
            matches!(error, LockError::Io { .. }),
            "unexpected error: {error}"
        );
    }
}
