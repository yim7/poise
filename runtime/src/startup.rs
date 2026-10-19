//! 启动文件与启动校验（设计第 2.0、10 节，B31、B47、B48）。
//!
//! 启动文件是实例目录里的 `poise.toml`，由用户自己编写，程序只读不写：
//!
//! ```toml
//! network = "testnet"
//! account_address = "0x…"
//! api_wallet_private_key = "0x…"
//! data_dir = "data"
//! listen = "127.0.0.1:8010"
//! ```
//!
//!
//! 校验不止「读到了」，而是「可用」：权限必须是 600、私钥格式必须正确、由私钥
//! 推出的地址必须在这个账户已授权且未过期的 API 钱包里、账户模式必须是 Unified。
//! 每一项失败都单独报出来，说明是哪一项、哪一步。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use poise_strategy::units::Notional;
use serde::Deserialize;

use crate::account::{Account, AccountMode, ApiWallet};
use crate::exchange::ExchangeError;
use crate::secret::{Address, SecretError, SecretKey};

/// API 钱包剩余有效期少于这个天数时提醒用户续期（设计附录 A）。
pub const API_WALLET_WARNING_DAYS: i64 = 14;

/// 数据库文件名。放在启动文件指定的 `data_dir` 下。
pub const DATABASE_FILE_NAME: &str = "poise.sqlite";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Testnet,
    Mainnet,
}

impl Network {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Testnet => "testnet",
            Self::Mainnet => "mainnet",
        }
    }
}

/// 读进来并做过格式校验的启动文件。
///
/// `Debug` 是安全的：私钥字段的类型自己不显示内容。
#[derive(Debug)]
pub struct StartupFile {
    network: Network,
    account_address: Address,
    api_wallet_private_key: SecretKey,
    data_dir: PathBuf,
    listen: String,
    path: PathBuf,
}

impl StartupFile {
    /// 读取并校验启动文件本身：权限、字段、私钥格式、网络取值。
    ///
    /// 只读文件，不创建目录：数据目录由调用方按需创建。
    pub fn load(path: impl AsRef<Path>) -> Result<Self, StartupError> {
        let path = path.as_ref().to_path_buf();
        let raw = Self::open(&path)?;

        let network = match required(raw.network.as_deref(), "network")? {
            "testnet" => Network::Testnet,
            "mainnet" => Network::Mainnet,
            other => {
                return Err(StartupError::UnknownNetwork {
                    value: other.to_string(),
                });
            }
        };
        let account_address =
            Address::parse(required(raw.account_address.as_deref(), "account_address")?).map_err(
                |source| StartupError::InvalidField {
                    field: "account_address",
                    source,
                },
            )?;
        let api_wallet_private_key = SecretKey::parse(required(
            raw.api_wallet_private_key.as_deref(),
            "api_wallet_private_key",
        )?)
        .map_err(|source| StartupError::InvalidField {
            field: "api_wallet_private_key",
            source,
        })?;
        let data_dir = PathBuf::from(required(raw.data_dir.as_deref(), "data_dir")?);
        let data_dir = match path.parent() {
            Some(parent) if data_dir.is_relative() => parent.join(data_dir),
            _ => data_dir,
        };
        let listen = required(raw.listen.as_deref(), "listen")?.to_string();

        Ok(Self {
            network,
            account_address,
            api_wallet_private_key,
            data_dir,
            listen,
            path,
        })
    }

    fn open(path: &Path) -> Result<RawStartupFile, StartupError> {
        check_permissions(path)?;
        let text = std::fs::read_to_string(path).map_err(|source| StartupError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|error| {
            // 只留位置和解析器的说明。`toml::de::Error` 的 Display 和 Debug 都会把
            // 出错的那一行（以及整份文件）原样回显，而那一行很可能就是私钥。
            StartupError::Toml {
                path: path.to_path_buf(),
                position: error
                    .span()
                    .map(|span| position_in(&text, span.start))
                    .unwrap_or_else(|| "an unknown position".to_string()),
                message: error.message().to_string(),
            }
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn network(&self) -> Network {
        self.network
    }

    pub fn account_address(&self) -> Address {
        self.account_address
    }

    /// 由私钥推出的 API 钱包地址。它就是签名时用的那个钱包。
    pub fn api_wallet_address(&self) -> Address {
        self.api_wallet_private_key.address()
    }

    pub fn api_wallet_private_key(&self) -> &SecretKey {
        &self.api_wallet_private_key
    }

    /// 数据目录，已按启动文件所在目录解析成绝对路径。
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join(DATABASE_FILE_NAME)
    }

    pub fn listen(&self) -> &str {
        &self.listen
    }
}

/// 启动校验的结果：可以用来显示，也是「确实可用」的证据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupCheck {
    /// 由私钥推出的 API 钱包地址。
    pub api_wallet_address: Address,
    pub account_mode: AccountMode,
    /// 这个 API 钱包的授权到期时间。
    pub api_wallet_valid_until: DateTime<Utc>,
    /// 可用 USDC。
    pub available_collateral: Notional,
    pub warnings: Vec<StartupWarning>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupWarning {
    /// API 钱包快要过期了。
    ApiWalletExpiresSoon {
        address: Address,
        valid_until: DateTime<Utc>,
        remaining_days: i64,
    },
}

/// 用交易所的答案完成启动校验（设计第 2.0 节）。
///
/// 顺序：账户模式 → 私钥推出的地址是不是已授权的 API 钱包 → 可用余额。
/// 任何一项不成立都拒绝启动并说明是哪一项。
pub async fn validate(
    file: &StartupFile,
    account: &dyn Account,
    now: DateTime<Utc>,
) -> Result<StartupCheck, StartupError> {
    let account_mode = account.mode().await?;
    if account_mode != AccountMode::Unified {
        return Err(StartupError::AccountNotUnified {
            mode: account_mode.as_str().to_string(),
        });
    }

    let api_wallet_address = file.api_wallet_address();
    let wallet = find_api_wallet(
        account.authorized_api_wallets().await?,
        api_wallet_address,
        now,
    )?;

    Ok(StartupCheck {
        api_wallet_address,
        account_mode,
        api_wallet_valid_until: wallet.valid_until,
        available_collateral: account.available_collateral().await?,
        warnings: expiry_warnings(&wallet, now),
    })
}

fn find_api_wallet(
    wallets: Vec<ApiWallet>,
    address: Address,
    now: DateTime<Utc>,
) -> Result<ApiWallet, StartupError> {
    let Some(wallet) = wallets.into_iter().find(|wallet| wallet.address == address) else {
        return Err(StartupError::ApiWalletNotAuthorized { address });
    };
    if wallet.valid_until <= now {
        return Err(StartupError::ApiWalletExpired {
            address,
            valid_until: wallet.valid_until,
        });
    }
    Ok(wallet)
}

fn expiry_warnings(wallet: &ApiWallet, now: DateTime<Utc>) -> Vec<StartupWarning> {
    let remaining = wallet.valid_until - now;
    if remaining >= Duration::days(API_WALLET_WARNING_DAYS) {
        return Vec::new();
    }
    vec![StartupWarning::ApiWalletExpiresSoon {
        address: wallet.address,
        valid_until: wallet.valid_until,
        remaining_days: remaining.num_days(),
    }]
}

/// 启动文件只允许当前用户碰：同组和其他人不得有任何权限位。
///
/// 600（读写）和 400（只读）都行——只读的启动文件照样能跑，没必要为了 600 去改它。
#[cfg(unix)]
fn check_permissions(path: &Path) -> Result<(), StartupError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path).map_err(|source| StartupError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(StartupError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

/// 非 Unix 平台没有这套权限位，启动文件保护的判断留给用户。
#[cfg(not(unix))]
fn check_permissions(_path: &Path) -> Result<(), StartupError> {
    Ok(())
}

/// 字节偏移 → `line L column C`，行列都从 1 开始。
///
/// 只报告位置，不回显那一段原文：它可能就是私钥。
fn position_in(text: &str, offset: usize) -> String {
    let mut end = offset.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let before = &text[..end];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rfind('\n')
        .map_or(before.len(), |newline| before.len() - newline - 1)
        + 1;
    format!("line {line} column {column}")
}

fn required<'a>(value: Option<&'a str>, field: &'static str) -> Result<&'a str, StartupError> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(StartupError::MissingField { field })
}

/// 刻意不实现 `Debug`：`api_wallet_private_key` 在这里还是一段明文，
/// 任何打日志或断言失败都可能把它带出去。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStartupFile {
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    account_address: Option<String>,
    #[serde(default)]
    api_wallet_private_key: Option<String>,
    #[serde(default)]
    data_dir: Option<String>,
    #[serde(default)]
    listen: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("cannot read the startup file `{path}`: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the startup file `{path}` is not valid TOML at {position}: {message}")]
    Toml {
        path: PathBuf,
        /// 形如 `line 3 column 26`，或 `an unknown position`。
        position: String,
        /// 解析器的说明，**不含**出错那一行的原文。
        message: String,
    },
    #[error(
        "the startup file `{path}` has permissions {mode:o}; the group and other users must have no access at all, so only the owner's permission bits may be set (400 and 600 both work)"
    )]
    InsecurePermissions { path: PathBuf, mode: u32 },
    #[error("the startup file is missing `{field}`")]
    MissingField { field: &'static str },
    #[error("`{field}` in the startup file is invalid: {source}")]
    InvalidField {
        field: &'static str,
        source: SecretError,
    },
    #[error("unknown network `{value}`; expected `testnet` or `mainnet`")]
    UnknownNetwork { value: String },
    #[error(
        "the account is in `{mode}` mode, not `unifiedAccount`; Poise supports only the Unified account and does not switch modes for you"
    )]
    AccountNotUnified { mode: String },
    #[error(
        "the address derived from `api_wallet_private_key` ({address}) is not an authorized API wallet of this account"
    )]
    ApiWalletNotAuthorized { address: Address },
    #[error("the API wallet {address} expired at {valid_until}")]
    ApiWalletExpired {
        address: Address,
        valid_until: DateTime<Utc>,
    },
    #[error(transparent)]
    Exchange(#[from] ExchangeError),
}
