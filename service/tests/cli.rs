use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use tempfile::TempDir;

#[test]
fn help_flag_prints_usage_and_exits_promptly() -> Result<()> {
    let output = run_cli_and_wait(&["--help"])?;
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("grid-platform-service"));

    Ok(())
}

#[test]
fn version_flag_prints_version_and_exits_promptly() -> Result<()> {
    let output = run_cli_and_wait(&["--version"])?;
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains(env!("CARGO_PKG_VERSION")));
    assert!(stdout.contains("grid-platform-service"));

    Ok(())
}

#[test]
fn config_flag_reads_file_and_reports_missing_path_promptly() -> Result<()> {
    let output = run_cli_and_wait(&["--config", "tests/fixtures/does-not-exist.toml"])?;
    assert!(!output.status.success());

    let stderr = String::from_utf8(output.stderr)?;
    assert!(stderr.contains("--config"));
    assert!(stderr.contains("does-not-exist.toml"));

    Ok(())
}

#[test]
fn mainnet_requires_explicit_opt_in() -> Result<()> {
    let output = run_cli_and_wait_with_env(
        &[],
        &[
            ("GRID_PLATFORM_SERVICE_ADDR", "127.0.0.1:0"),
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
        ],
    )?;

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("GRID_PLATFORM_ALLOW_MAINNET=1"));
    Ok(())
}

#[test]
fn mainnet_rejects_non_one_allow_flag_value() -> Result<()> {
    let output = run_cli_and_wait_with_env(
        &[],
        &[
            ("GRID_PLATFORM_SERVICE_ADDR", "127.0.0.1:0"),
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
            ("GRID_PLATFORM_ALLOW_MAINNET", "true"),
        ],
    )?;

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("GRID_PLATFORM_ALLOW_MAINNET=1"));
    Ok(())
}

#[test]
fn mainnet_requires_signed_startup_state() -> Result<()> {
    let output = run_cli_and_wait_with_env(
        &[],
        &[
            ("GRID_PLATFORM_SERVICE_ADDR", "127.0.0.1:0"),
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
            ("GRID_PLATFORM_ALLOW_MAINNET", "1"),
        ],
    )?;

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("STARTUP_MAINNET_SIGNED_STATE_UNAVAILABLE"));
    Ok(())
}

#[test]
fn dotenv_file_is_loaded_before_startup_config_is_built() -> Result<()> {
    let temp = TempDir::new()?;
    write_dotenv(
        temp.path(),
        r#"
GRID_PLATFORM_SERVICE_ADDR=127.0.0.1:0
GRID_PLATFORM_BINANCE_ENABLED=1
GRID_PLATFORM_BINANCE_ENV=mainnet
GRID_PLATFORM_ALLOW_MAINNET=1
"#,
    )?;

    let output = run_cli_and_wait_in_dir_with_env(&[], &[], temp.path())?;

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("STARTUP_MAINNET_SIGNED_STATE_UNAVAILABLE"));
    Ok(())
}

#[test]
fn process_env_overrides_dotenv_values() -> Result<()> {
    let temp = TempDir::new()?;
    write_dotenv(
        temp.path(),
        r#"
GRID_PLATFORM_SERVICE_ADDR=127.0.0.1:0
GRID_PLATFORM_BINANCE_ENABLED=1
GRID_PLATFORM_BINANCE_ENV=mainnet
GRID_PLATFORM_ALLOW_MAINNET=true
"#,
    )?;

    let output = run_cli_and_wait_in_dir_with_env(
        &[],
        &[
            ("GRID_PLATFORM_SERVICE_ADDR", "127.0.0.1:0"),
            ("GRID_PLATFORM_BINANCE_ENABLED", "1"),
            ("GRID_PLATFORM_BINANCE_ENV", "mainnet"),
            ("GRID_PLATFORM_ALLOW_MAINNET", "1"),
        ],
        temp.path(),
    )?;

    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)?.contains("STARTUP_MAINNET_SIGNED_STATE_UNAVAILABLE"));
    Ok(())
}

fn run_cli_and_wait(args: &[&str]) -> Result<Output> {
    run_cli_and_wait_with_env(args, &[])
}

fn run_cli_and_wait_with_env(args: &[&str], envs: &[(&str, &str)]) -> Result<Output> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_grid-platform-service"))
        .args(args)
        .env("GRID_PLATFORM_SERVICE_ADDR", "127.0.0.1:0")
        .envs(envs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map_err(Into::into);
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            return Err(anyhow!(
                "CLI process did not exit within {:?} for args {:?}",
                Duration::from_secs(2),
                args
            ));
        }
        sleep(Duration::from_millis(25));
    }
}

fn run_cli_and_wait_in_dir_with_env(
    args: &[&str],
    envs: &[(&str, &str)],
    cwd: &Path,
) -> Result<Output> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_grid-platform-service"))
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(envs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map_err(Into::into);
        }
        if Instant::now() >= deadline {
            child.kill().ok();
            child.wait().ok();
            return Err(anyhow!(
                "CLI process did not exit within {:?} for args {:?} in {:?}",
                Duration::from_secs(2),
                args,
                cwd
            ));
        }
        sleep(Duration::from_millis(25));
    }
}

fn write_dotenv(dir: &Path, content: &str) -> Result<()> {
    fs::write(dir.join(".env"), content.trim_start())?;
    Ok(())
}
