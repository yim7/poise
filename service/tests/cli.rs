use std::{
    process::{Command, Output, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};

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
    assert!(String::from_utf8(output.stderr)?
        .contains("STARTUP_MAINNET_SIGNED_STATE_UNAVAILABLE"));
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
