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

fn run_cli_and_wait(args: &[&str]) -> Result<Output> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_grid-platform-service"))
        .args(args)
        .env("GRID_PLATFORM_SERVICE_ADDR", "127.0.0.1:0")
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
