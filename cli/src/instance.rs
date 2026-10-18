//! 实例目录：启动文件、数据库和交易所客户端。
//!
//! 实例目录由启动参数指定，里面只有一个程序读、用户写的 `poise.toml`。
//! 连哪个网络由启动文件决定：正式网要用户自己把正式网 API 钱包私钥写进去，这就是
//! 正式网的门槛，程序里不再另设一道。

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use poise_hyperliquid::{Hyperliquid, HyperliquidConfig};
use poise_runtime::{Account, Exchange, InstanceLock, Runtime, RuntimeOptions, StartupFile, Store};

/// 启动文件名，固定放在实例目录下。
pub const STARTUP_FILE_NAME: &str = "poise.toml";

/// 常驻服务实际监听的地址，服务起来之后写在数据目录里，退出时删掉。
///
/// `poise stop` 在服务运行时按它去连，不按启动文件里的 `listen`：服务可以用 `--listen`
/// 换端口，按启动文件去连可能连到同一端口上的别的服务（旧系统就在 8000）。
pub const LISTEN_FILE_NAME: &str = "poise.listen";

pub struct Instance {
    startup: StartupFile,
    exchange: Arc<Hyperliquid>,
}

impl Instance {
    /// 读启动文件、准备数据目录和交易所客户端。不发任何请求。
    pub fn load(directory: &Path) -> Result<Self> {
        let path = directory.join(STARTUP_FILE_NAME);
        let startup = StartupFile::load(&path)
            .with_context(|| format!("cannot load the startup file `{}`", path.display()))?;
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
        //
        // 同时确认数据库属于这个网络和账户：同一个数据目录换了网络或账户，测试网的策略
        // 不能拿到正式网上恢复运行。
        instance
            .open_store()?
            .bind_identity(
                instance.startup.network().as_str(),
                &instance.startup.account_address().to_string(),
            )
            .with_context(|| {
                format!(
                    "the database `{}` does not belong to this instance",
                    instance.startup.database_path().display()
                )
            })?;
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

    /// 常驻服务写下实际监听地址的文件（[`LISTEN_FILE_NAME`]）。
    pub fn listen_file(&self) -> std::path::PathBuf {
        self.startup.data_dir().join(LISTEN_FILE_NAME)
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

    /// 正式网和测试网一样加载：网络按启动文件，交易所客户端指向正式网。加载不发请求。
    #[test]
    fn accepts_a_mainnet_instance() {
        let directory = tempfile::tempdir().unwrap();
        write_startup(directory.path(), "mainnet");

        let instance = Instance::load(directory.path()).unwrap();

        assert_eq!(
            instance.startup().network(),
            poise_runtime::Network::Mainnet
        );
    }

    /// 同一个数据目录换了网络：拒绝加载，测试网的策略不能拿到正式网上恢复运行。
    #[test]
    fn refuses_to_reuse_a_testnet_database_on_mainnet() {
        let directory = tempfile::tempdir().unwrap();
        write_startup(directory.path(), "testnet");
        Instance::load(directory.path()).unwrap();

        write_startup(directory.path(), "mainnet");
        let Err(error) = Instance::load(directory.path()) else {
            panic!("a testnet database must not be reused on mainnet");
        };

        assert!(
            format!("{error:#}").contains("separate data directory"),
            "{error:#}"
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
