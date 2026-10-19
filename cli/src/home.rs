//! 实例放在哪里：根目录下每个实例一个子目录（`~/.poise/testnet/`、`~/.poise/mainnet/`），
//! 里面是启动文件 `poise.toml` 和数据目录。
//!
//! `--instance` 写名字就到根目录下找；写路径（含 `/`，或以 `.`、`~` 开头）就照原样用，
//! 实例可以放在任何地方。根目录缺省是 `~/.poise`，环境变量 `POISE_HOME` 可以换。
//!
//! 不写 `--instance` 时报错并列出已有实例，不默认当前目录：正式网实例的私钥就在启动文件
//! 里，宁可多写一个名字，也不能因为在哪个目录下执行而连错实例。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use poise_runtime::StartupFile;

use crate::instance::STARTUP_FILE_NAME;

/// 指定根目录的环境变量。
pub const HOME_VARIABLE: &str = "POISE_HOME";

/// 根目录：`POISE_HOME`，没设时是用户目录下的 `.poise`。
pub fn root(variable: Option<OsString>, user_home: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(root) = variable.filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    match user_home {
        Some(home) => Ok(home.join(".poise")),
        None => bail!("找不到用户目录：用 {HOME_VARIABLE} 指定实例根目录"),
    }
}

/// 进程环境下的根目录。
pub fn root_from_env() -> Result<PathBuf> {
    root(
        std::env::var_os(HOME_VARIABLE),
        std::env::var_os("HOME").map(PathBuf::from),
    )
}

/// `--instance` 指向的实例目录。
pub fn resolve(instance: Option<&str>, root: &Path) -> Result<PathBuf> {
    let Some(value) = instance else {
        bail!("要用 --instance 指定实例。{}", available(root));
    };
    if value.contains('/') || value.starts_with('.') || value.starts_with('~') {
        return Ok(PathBuf::from(value));
    }
    let directory = root.join(value);
    if !directory.join(STARTUP_FILE_NAME).is_file() {
        bail!(
            "没有名为 `{value}` 的实例（{} 下没有 {STARTUP_FILE_NAME}）。{}",
            directory.display(),
            available(root)
        );
    }
    Ok(directory)
}

/// 根目录下的实例名：有启动文件的子目录，按名字排序。
pub fn names(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().join(STARTUP_FILE_NAME).is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// 报错里的一句「已有哪些实例」。
fn available(root: &Path) -> String {
    let names = names(root);
    if names.is_empty() {
        format!(
            "{} 下还没有实例：建一个目录，放进 {STARTUP_FILE_NAME}。",
            root.display()
        )
    } else {
        format!("{} 下已有：{}。", root.display(), names.join("、"))
    }
}

/// `poise instances` 的一行：实例名、网络、监听地址；启动文件读不了时是原因。不显示私钥。
pub fn describe(root: &Path) -> Vec<String> {
    names(root)
        .into_iter()
        .map(
            |name| match StartupFile::load(root.join(&name).join(STARTUP_FILE_NAME)) {
                Ok(startup) => format!(
                    "{name}\t{}\t{}\t{}",
                    startup.network().as_str(),
                    startup.listen(),
                    startup.account_address()
                ),
                Err(error) => format!("{name}\t启动文件读不了：{error}"),
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::{describe, names, resolve, root};
    use crate::instance::STARTUP_FILE_NAME;

    fn write_startup(root: &Path, name: &str, network: &str, listen: &str) {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join(STARTUP_FILE_NAME);
        std::fs::write(
            &path,
            format!(
                "network = \"{network}\"\n\
                 account_address = \"0x2222222222222222222222222222222222222222\"\n\
                 api_wallet_private_key = \"0x0000000000000000000000000000000000000000000000000000000000000001\"\n\
                 data_dir = \"data\"\n\
                 listen = \"{listen}\"\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn the_root_is_poise_home_or_dot_poise_under_the_user_home() {
        assert_eq!(
            root(Some("/srv/poise".into()), Some(PathBuf::from("/Users/me"))).unwrap(),
            PathBuf::from("/srv/poise")
        );
        assert_eq!(
            root(None, Some(PathBuf::from("/Users/me"))).unwrap(),
            PathBuf::from("/Users/me/.poise")
        );
        // 设成空字符串等于没设。
        assert_eq!(
            root(Some("".into()), Some(PathBuf::from("/Users/me"))).unwrap(),
            PathBuf::from("/Users/me/.poise")
        );
        assert!(root(None, None).is_err());
    }

    #[test]
    fn a_name_is_looked_up_under_the_root() {
        let home = tempfile::tempdir().unwrap();
        write_startup(home.path(), "testnet", "testnet", "127.0.0.1:8010");

        assert_eq!(
            resolve(Some("testnet"), home.path()).unwrap(),
            home.path().join("testnet")
        );
    }

    #[test]
    fn a_path_is_used_as_given() {
        let home = tempfile::tempdir().unwrap();

        assert_eq!(
            resolve(Some("/elsewhere/mainnet"), home.path()).unwrap(),
            PathBuf::from("/elsewhere/mainnet")
        );
        assert_eq!(
            resolve(Some("./local"), home.path()).unwrap(),
            PathBuf::from("./local")
        );
    }

    /// 不写、或者写了不存在的名字：报错，并列出已有的实例。
    #[test]
    fn a_missing_instance_lists_the_ones_that_exist() {
        let home = tempfile::tempdir().unwrap();
        write_startup(home.path(), "testnet", "testnet", "127.0.0.1:8010");
        write_startup(home.path(), "mainnet", "mainnet", "127.0.0.1:8011");
        // 没有启动文件的目录不算实例。
        std::fs::create_dir_all(home.path().join("scratch")).unwrap();

        let unnamed = resolve(None, home.path()).unwrap_err().to_string();
        assert!(unnamed.contains("--instance"), "{unnamed}");
        assert!(unnamed.contains("mainnet、testnet"), "{unnamed}");

        let unknown = resolve(Some("mainet"), home.path())
            .unwrap_err()
            .to_string();
        assert!(unknown.contains("`mainet`"), "{unknown}");
        assert!(unknown.contains("mainnet、testnet"), "{unknown}");
        assert!(!unknown.contains("scratch"), "{unknown}");
    }

    #[test]
    fn an_empty_root_says_how_to_create_an_instance() {
        let home = tempfile::tempdir().unwrap();

        let error = resolve(None, home.path()).unwrap_err().to_string();

        assert!(error.contains("还没有实例"), "{error}");
        assert_eq!(names(&home.path().join("missing")), Vec::<String>::new());
    }

    /// 列出实例：网络、监听地址和账户，不显示私钥；读不了的说原因。
    #[test]
    fn instances_are_described_without_the_private_key() {
        let home = tempfile::tempdir().unwrap();
        write_startup(home.path(), "mainnet", "mainnet", "127.0.0.1:8011");
        write_startup(home.path(), "testnet", "testnet", "127.0.0.1:8010");
        std::fs::set_permissions(
            home.path().join("testnet").join(STARTUP_FILE_NAME),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        let lines = describe(home.path());

        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].starts_with("mainnet\tmainnet\t127.0.0.1:8011\t0x2222"),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("testnet\t启动文件读不了"),
            "{}",
            lines[1]
        );
        assert!(lines.iter().all(|line| !line.contains("0000000000000001")));
    }
}
