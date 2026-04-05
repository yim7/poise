use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{suffix}", std::process::id()))
}

fn write_temp_crate(dir: &Path, body: &str) {
    fs::create_dir_all(dir.join("src")).expect("create temp crate src dir");
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "poise-binance-public-api-check"
version = "0.1.0"
edition = "2024"

[dependencies]
poise-binance = {{ path = "{}" }}
"#,
            env!("CARGO_MANIFEST_DIR")
        ),
    )
    .expect("write temp cargo manifest");
    fs::write(dir.join("src/main.rs"), body).expect("write temp main.rs");
}

fn cargo_check(dir: &Path) -> std::process::Output {
    Command::new("cargo")
        .arg("check")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .output()
        .expect("run cargo check for temp crate")
}

#[test]
fn crate_root_reexports_binance_adapter() {
    let dir = unique_temp_dir("poise-binance-public-api-pass");
    write_temp_crate(
        &dir,
        r#"use poise_binance::BinanceAdapter;

fn main() {
    let _ = std::mem::size_of::<BinanceAdapter>();
}
"#,
    );

    let output = cargo_check(&dir);
    if !output.status.success() {
        panic!(
            "expected crate root import to compile, stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fs::remove_dir_all(&dir).expect("remove temp crate");
}

#[test]
fn internal_modules_are_not_public_api() {
    let dir = unique_temp_dir("poise-binance-public-api-fail");
    write_temp_crate(
        &dir,
        r#"use poise_binance::adapter::BinanceAdapter;
use poise_binance::rest::BinanceRestClient;
use poise_binance::types::BinanceAccountSummaryInformation;
use poise_binance::websocket::BinanceWsClient;

fn main() {
    let _ = std::mem::size_of::<BinanceAdapter>();
    let _ = std::mem::size_of::<BinanceRestClient>();
    let _ = std::mem::size_of::<BinanceAccountSummaryInformation>();
    let _ = std::mem::size_of::<BinanceWsClient>();
}
"#,
    );

    let output = cargo_check(&dir);
    assert!(
        !output.status.success(),
        "expected internal modules to stay private, but cargo check succeeded"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("private module")
            || stderr.contains("module `adapter` is private")
            || stderr.contains("module `rest` is private")
            || stderr.contains("module `types` is private")
            || stderr.contains("module `websocket` is private"),
        "expected private module error, stderr:\n{stderr}"
    );

    fs::remove_dir_all(&dir).expect("remove temp crate");
}
