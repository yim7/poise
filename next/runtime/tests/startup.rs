//! 启动文件与启动校验（设计第 2.0、10 节；B31、B47、B48）。
//!
//! 全部离线：交易所那一侧用一个假的 [`Account`]。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use poise_runtime::account::{Account, AccountMode, ApiWallet};
use poise_runtime::exchange::ExchangeError;
use poise_runtime::secret::{Address, SecretKey};
use poise_runtime::startup::{
    API_WALLET_WARNING_DAYS, StartupError, StartupFile, StartupWarning, validate,
};
use poise_strategy::instrument::{Fees, Instrument, InstrumentSpec};
use poise_strategy::units::{Notional, Price, Quantity, Rate};

/// 私钥 0x…01 与其公开的地址。
const KEY: &str = "0x0000000000000000000000000000000000000000000000000000000000000001";
const KEY_ADDRESS: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
const ACCOUNT: &str = "0x2222222222222222222222222222222222222222";

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 15, 0, 0, 0).unwrap() + Duration::seconds(seconds)
}

fn address() -> Address {
    Address::parse(KEY_ADDRESS).unwrap()
}

struct FakeAccount {
    mode: AccountMode,
    wallets: Vec<ApiWallet>,
    available: Notional,
}

impl FakeAccount {
    fn unified(wallets: Vec<ApiWallet>) -> Self {
        Self {
            mode: AccountMode::Unified,
            wallets,
            available: Notional::parse("870.25").unwrap(),
        }
    }
}

#[async_trait]
impl Account for FakeAccount {
    async fn mode(&self) -> Result<AccountMode, ExchangeError> {
        Ok(self.mode.clone())
    }

    async fn authorized_api_wallets(&self) -> Result<Vec<ApiWallet>, ExchangeError> {
        Ok(self.wallets.clone())
    }

    async fn available_collateral(&self) -> Result<Notional, ExchangeError> {
        Ok(self.available)
    }

    async fn fees(&self) -> Result<Fees, ExchangeError> {
        Ok(Fees::new(
            Rate::parse("0.00045").unwrap(),
            Rate::parse("0.00015").unwrap(),
        ))
    }

    async fn instrument_spec(
        &self,
        instrument: &Instrument,
    ) -> Result<InstrumentSpec, ExchangeError> {
        InstrumentSpec::new(
            instrument.clone(),
            Price::parse("1").unwrap(),
            Quantity::parse("0.001").unwrap(),
            Notional::parse("10").unwrap(),
        )
        .map_err(|error| ExchangeError::Unavailable {
            reason: error.to_string(),
        })
    }
}

fn wallet(valid_until: DateTime<Utc>) -> ApiWallet {
    ApiWallet {
        name: "poise".to_string(),
        address: address(),
        valid_until,
    }
}

/// 写一个启动文件并设成 600。
fn write_startup(directory: &Path, private_key: &str) -> PathBuf {
    let path = directory.join("poise.toml");
    std::fs::write(
        &path,
        format!(
            "network = \"testnet\"\n\
             account_address = \"{ACCOUNT}\"\n\
             api_wallet_private_key = \"{private_key}\"\n\
             data_dir = \"data\"\n\
             listen = \"127.0.0.1:8000\"\n"
        ),
    )
    .unwrap();
    set_mode(&path, 0o600);
    path
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

fn load(directory: &Path, private_key: &str) -> Result<StartupFile, StartupError> {
    StartupFile::load(write_startup(directory, private_key))
}

#[test]
fn reads_every_field_and_resolves_the_data_directory_next_to_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let subdirectory = directory.path().join("instance");
    std::fs::create_dir(&subdirectory).unwrap();

    let file = load(&subdirectory, KEY).unwrap();

    assert_eq!(file.network().as_str(), "testnet");
    assert_eq!(file.account_address().to_string(), ACCOUNT);
    assert_eq!(file.api_wallet_address().to_string(), KEY_ADDRESS);
    assert_eq!(file.data_dir(), subdirectory.join("data"));
    assert_eq!(
        file.database_path(),
        subdirectory.join("data").join("poise.sqlite")
    );
    assert_eq!(file.listen(), "127.0.0.1:8000");
    assert_eq!(file.path(), subdirectory.join("poise.toml"));
}

#[test]
fn b48_rejects_a_startup_file_that_is_readable_by_others() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_startup(directory.path(), KEY);
    set_mode(&path, 0o644);

    let error = StartupFile::load(&path).unwrap_err();

    assert!(
        matches!(error, StartupError::InsecurePermissions { mode: 0o644, .. }),
        "unexpected error: {error}"
    );
    // 报错要说清楚什么才算合规，不能只说「不对」。
    let message = error.to_string();
    assert!(
        message.contains("400 and 600"),
        "unexpected message: {message}"
    );
}

/// 只读的启动文件照样能跑：600 和 400 都接受，同组和其他人一律不行。
#[test]
fn b48_accepts_a_read_only_startup_file_and_still_refuses_group_access() {
    let directory = tempfile::tempdir().unwrap();
    let path = write_startup(directory.path(), KEY);

    set_mode(&path, 0o400);
    assert!(StartupFile::load(&path).is_ok(), "400 应当被接受");

    set_mode(&path, 0o600);
    assert!(StartupFile::load(&path).is_ok(), "600 应当被接受");

    // 只要同组和其他人一个权限位都没有，所有者自己有几位都行。
    set_mode(&path, 0o700);
    assert!(StartupFile::load(&path).is_ok(), "700 应当被接受");

    // 多出任何一个「同组/其他人」的权限位都要拒绝，哪怕所有者仍然可读。
    for mode in [0o640, 0o604, 0o660, 0o606, 0o770] {
        set_mode(&path, mode);
        let error = StartupFile::load(&path).unwrap_err();
        assert!(
            matches!(error, StartupError::InsecurePermissions { .. }),
            "{mode:o} 应当被拒绝，实际是 {error}"
        );
    }
}

#[test]
fn b48_rejects_a_missing_or_empty_private_key_field() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("poise.toml");
    std::fs::write(
        &path,
        format!(
            "network = \"testnet\"\n\
             account_address = \"{ACCOUNT}\"\n\
             data_dir = \"data\"\n\
             listen = \"127.0.0.1:8000\"\n"
        ),
    )
    .unwrap();
    set_mode(&path, 0o600);

    let error = StartupFile::load(&path).unwrap_err();

    assert_eq!(
        error.to_string(),
        "the startup file is missing `api_wallet_private_key`"
    );

    let empty = load(directory.path(), "  ").unwrap_err();
    assert!(
        matches!(
            empty,
            StartupError::MissingField {
                field: "api_wallet_private_key"
            }
        ),
        "unexpected error: {empty}"
    );
}

#[test]
fn b48_rejects_a_private_key_that_is_not_a_32_byte_hex_scalar() {
    let directory = tempfile::tempdir().unwrap();

    let error = load(directory.path(), "0xnot-a-key").unwrap_err();

    assert!(
        matches!(
            error,
            StartupError::InvalidField {
                field: "api_wallet_private_key",
                ..
            }
        ),
        "unexpected error: {error}"
    );
    assert!(!error.to_string().contains("not-a-key"));
}

#[test]
fn b47_the_private_key_never_appears_in_debug_output_or_errors() {
    let directory = tempfile::tempdir().unwrap();

    let debug = format!("{:?}", load(directory.path(), KEY).unwrap());

    assert!(debug.contains("SecretKey(<redacted>)"));
    assert!(
        !debug.contains("0000000000000000000000000000000000000000000000000000000000000001"),
        "the private key leaked into the debug output: {debug}"
    );
    // 私钥本身没有 Display，也没有任何返回私钥原文的方法。
    let key = SecretKey::parse(KEY).unwrap();
    assert_eq!(format!("{key:?}"), "SecretKey(<redacted>)");
}

/// TOML 解析失败时不能把出错的那一行回显出来——那一行就是私钥。
#[test]
fn b47_a_toml_error_does_not_echo_the_private_key() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("poise.toml");
    // 少写一对引号：私钥这一行不是合法的 TOML，解析器的报错通常会带上这一行。
    std::fs::write(
        &path,
        format!(
            "network = \"testnet\"\n\
             account_address = \"{ACCOUNT}\"\n\
             api_wallet_private_key = {KEY}\n\
             data_dir = \"data\"\n\
             listen = \"127.0.0.1:8000\"\n"
        ),
    )
    .unwrap();
    set_mode(&path, 0o600);

    let error = StartupFile::load(&path).unwrap_err();
    let shown = format!("{error} / {error:?}");

    assert!(
        matches!(error, StartupError::Toml { .. }),
        "unexpected error: {shown}"
    );
    assert!(
        !shown.contains("0000000000000000000000000000000000000000000000000000000000000001"),
        "the private key leaked into the error: {shown}"
    );
    // 说明位置仍然要有，否则用户只能猜是哪一行写错了。
    assert!(
        shown.contains("line 3"),
        "the error should point at the line: {shown}"
    );
}

#[test]
fn rejects_a_startup_file_with_unknown_fields_or_unknown_network() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("poise.toml");
    std::fs::write(
        &path,
        format!(
            "network = \"devnet\"\n\
             account_address = \"{ACCOUNT}\"\n\
             api_wallet_private_key = \"{KEY}\"\n\
             data_dir = \"data\"\n\
             listen = \"127.0.0.1:8000\"\n"
        ),
    )
    .unwrap();
    set_mode(&path, 0o600);
    assert!(matches!(
        StartupFile::load(&path).unwrap_err(),
        StartupError::UnknownNetwork { .. }
    ));

    std::fs::write(
        &path,
        format!(
            "network = \"testnet\"\n\
             account_address = \"{ACCOUNT}\"\n\
             api_wallet_private_key = \"{KEY}\"\n\
             data_dir = \"data\"\n\
             listen = \"127.0.0.1:8000\"\n\
             unexpected = true\n"
        ),
    )
    .unwrap();
    assert!(matches!(
        StartupFile::load(&path).unwrap_err(),
        StartupError::Toml { .. }
    ));
}

#[tokio::test]
async fn accepts_a_unified_account_with_an_authorized_api_wallet() {
    let directory = tempfile::tempdir().unwrap();
    let file = load(directory.path(), KEY).unwrap();
    let account = FakeAccount::unified(vec![wallet(at(180 * 24 * 3600))]);

    let check = validate(&file, &account, at(0)).await.unwrap();

    assert_eq!(check.api_wallet_address.to_string(), KEY_ADDRESS);
    assert_eq!(check.account_mode, AccountMode::Unified);
    assert_eq!(
        check.available_collateral,
        Notional::parse("870.25").unwrap()
    );
    assert!(check.warnings.is_empty());
}

#[tokio::test]
async fn b31_rejects_an_account_that_is_not_unified_without_switching_it() {
    let directory = tempfile::tempdir().unwrap();
    let file = load(directory.path(), KEY).unwrap();
    let account = FakeAccount {
        mode: AccountMode::Other("default".to_string()),
        ..FakeAccount::unified(vec![wallet(at(180 * 24 * 3600))])
    };

    let error = validate(&file, &account, at(0)).await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "the account is in `default` mode, not `unifiedAccount`; Poise supports only the Unified account and does not switch modes for you"
    );
}

#[tokio::test]
async fn b48_rejects_a_private_key_whose_address_is_not_an_authorized_api_wallet() {
    let directory = tempfile::tempdir().unwrap();
    let file = load(directory.path(), KEY).unwrap();
    let other = ApiWallet {
        name: "someone-else".to_string(),
        address: Address::parse("0x3333333333333333333333333333333333333333").unwrap(),
        valid_until: at(180 * 24 * 3600),
    };
    let account = FakeAccount::unified(vec![other]);

    let error = validate(&file, &account, at(0)).await.unwrap_err();

    assert!(
        matches!(error, StartupError::ApiWalletNotAuthorized { .. }),
        "unexpected error: {error}"
    );
    // 说明是哪一项失败：写清楚是「推导出的地址不在已授权列表里」。
    assert!(error.to_string().contains(KEY_ADDRESS));
    assert!(
        error
            .to_string()
            .contains("is not an authorized API wallet")
    );
}

#[tokio::test]
async fn b48_rejects_an_api_wallet_that_has_expired() {
    let directory = tempfile::tempdir().unwrap();
    let file = load(directory.path(), KEY).unwrap();
    let account = FakeAccount::unified(vec![wallet(at(-1))]);

    let error = validate(&file, &account, at(0)).await.unwrap_err();

    assert!(
        matches!(error, StartupError::ApiWalletExpired { .. }),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn warns_when_the_api_wallet_expires_within_two_weeks() {
    let directory = tempfile::tempdir().unwrap();
    let file = load(directory.path(), KEY).unwrap();
    let soon = at(API_WALLET_WARNING_DAYS * 24 * 3600 - 60);
    let account = FakeAccount::unified(vec![wallet(soon)]);

    let check = validate(&file, &account, at(0)).await.unwrap();

    assert_eq!(
        check.warnings,
        vec![StartupWarning::ApiWalletExpiresSoon {
            address: address(),
            valid_until: soon,
            remaining_days: 13,
        }]
    );
}
