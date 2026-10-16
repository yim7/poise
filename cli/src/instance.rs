//! 实例目录：启动文件、数据库和交易所客户端。
//!
//! 实例目录由启动参数指定，里面只有一个程序读、用户写的 `poise.toml`。
//! 本期的命令只连测试网：读到 `network = "mainnet"` 就拒绝运行，任何请求都不发。

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use poise_hyperliquid::{Hyperliquid, HyperliquidConfig};
use poise_runtime::{
    Account, Exchange, InstanceLock, Network, Runtime, RuntimeOptions, StartupFile, Store,
};

/// 启动文件名，固定放在实例目录下。
pub const STARTUP_FILE_NAME: &str = "poise.toml";

pub struct Instance {
    startup: StartupFile,
    exchange: Arc<Hyperliquid>,
}

impl Instance {
    /// 读启动文件、检查网络、准备数据目录和交易所客户端。
    pub fn load(directory: &Path) -> Result<Self> {
        let path = directory.join(STARTUP_FILE_NAME);
        let startup = StartupFile::load(&path)
            .with_context(|| format!("cannot load the startup file `{}`", path.display()))?;
        if startup.network() == Network::Mainnet {
            bail!(
                "`{}` says `network = \"mainnet\"`; this build only talks to the testnet, so nothing was sent",
                path.display()
            );
        }

        std::fs::create_dir_all(startup.data_dir()).with_context(|| {
            format!(
                "cannot create the data directory `{}`",
                startup.data_dir().display()
            )
        })?;

        let exchange = Arc::new(Hyperliquid::new(HyperliquidConfig {
            network: startup.network(),
            account_address: startup.account_address(),
            api_wallet_private_key: startup.api_wallet_private_key().clone(),
            base_url: None,
        })?);

        tracing::info!(
            path = %path.display(),
            network = startup.network().as_str(),
            database = %startup.database_path().display(),
            "instance loaded"
        );
        let instance = Self { startup, exchange };
        // 打开一次数据库确认结构版本：对不上就在这里打住，`check`、`status`、`serve`
        // 都走这条启动路径。等写库那一刻才失败太晚——用户已经以为策略在跑了，
        // 而且报出来的是「少了一列」这种看不出该怎么办的话（第 12 节）。
        instance.open_store()?;
        Ok(instance)
    }

    pub fn startup(&self) -> &StartupFile {
        &self.startup
    }

    pub fn exchange(&self) -> &Arc<Hyperliquid> {
        &self.exchange
    }

    pub fn account(&self) -> &dyn Account {
        self.exchange.as_ref()
    }

    /// 取得实例锁：会写数据库、或者会向交易所下单撤单的命令，动手之前先取它。
    ///
    /// 一个实例只允许一个这样的进程，否则两个进程会各自规划、各自下单。
    /// 只读的命令（`check`、`status`）不取锁。
    pub fn lock(&self) -> Result<InstanceLock> {
        let data_dir = self.startup.data_dir();
        InstanceLock::acquire(data_dir)
            .with_context(|| format!("cannot take the instance lock in `{}`", data_dir.display()))
    }

    /// 打开数据库。命令各自开一次，用完即关。
    pub fn open_store(&self) -> Result<Store> {
        let path = self.startup.database_path();
        Store::open(&path).with_context(|| format!("cannot open the database `{}`", path.display()))
    }

    /// 用系统时钟建一个运行时，交易所和账户都是这个实例的客户端。
    pub fn runtime(&self) -> Result<Runtime> {
        let exchange: Arc<dyn Exchange> = self.exchange.clone();
        let account: Arc<dyn Account> = self.exchange.clone();
        Ok(Runtime::with_options(
            exchange,
            account,
            self.open_store()?,
            RuntimeOptions::default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::{Instance, STARTUP_FILE_NAME};

    /// 一份格式正确、权限正确的启动文件，网络按参数给。
    fn write_startup(directory: &std::path::Path, network: &str) -> std::path::PathBuf {
        let path = directory.join(STARTUP_FILE_NAME);
        std::fs::write(
            &path,
            format!(
                "network = \"{network}\"\n\
                 account_address = \"0x2222222222222222222222222222222222222222\"\n\
                 api_wallet_private_key = \"0x0000000000000000000000000000000000000000000000000000000000000001\"\n\
                 data_dir = \"data\"\n\
                 listen = \"127.0.0.1:8010\"\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    #[test]
    fn refuses_a_mainnet_instance_before_anything_else() {
        let directory = tempfile::tempdir().unwrap();
        write_startup(directory.path(), "mainnet");

        let Err(error) = Instance::load(directory.path()) else {
            panic!("a mainnet instance must be refused");
        };

        assert!(
            error.to_string().contains("mainnet"),
            "unexpected error: {error}"
        );
        assert!(
            !directory.path().join("data").exists(),
            "拒绝运行时不该动实例目录"
        );
    }

    #[test]
    fn accepts_a_testnet_instance() {
        let directory = tempfile::tempdir().unwrap();
        write_startup(directory.path(), "testnet");

        let instance = Instance::load(directory.path()).unwrap();

        assert_eq!(instance.startup().network().as_str(), "testnet");
        assert!(directory.path().join("data").is_dir());
    }

    #[test]
    fn refuses_a_startup_file_that_others_can_read() {
        let directory = tempfile::tempdir().unwrap();
        let path = write_startup(directory.path(), "testnet");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let Err(error) = Instance::load(directory.path()) else {
            panic!("a startup file others can read must be refused");
        };

        assert!(
            format!("{error:#}").contains("600"),
            "unexpected error: {error:#}"
        );
    }

    /// 结构版本对不上的旧库在启动时就报出来：`check`、`status`、`serve` 都会看到。
    ///
    /// 拖到写库那一刻才失败的话，用户已经以为策略在跑了（实测报的是「少了一列」）。
    #[test]
    fn refuses_a_database_whose_schema_version_is_not_this_one() {
        let directory = tempfile::tempdir().unwrap();
        write_startup(directory.path(), "testnet");
        // 一个建于「记结构版本」之前的旧库：有表，没有版本记录。
        let database = directory.path().join("data").join("poise.sqlite");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE batch_states (strategy_id TEXT NOT NULL);")
            .unwrap();
        drop(connection);

        let Err(error) = Instance::load(directory.path()) else {
            panic!("an old database must be refused");
        };

        let message = format!("{error:#}");
        assert!(
            message.contains(&database.display().to_string()),
            "错误里要写明库路径: {message}"
        );
        assert!(
            message.contains("schema version 0"),
            "错误里要写明库版本: {message}"
        );
        assert!(
            message.contains("move the old database away"),
            "错误里要说明怎么办: {message}"
        );
    }
}
