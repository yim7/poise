//! 一个实例只允许一个写进程：第二个进程拿不到实例锁就拒绝运行。
//!
//! 这里起的是真的第二个进程（`poise` 二进制），不是同一进程里的第二个句柄：
//! 文件锁是进程级的，只有真起一个进程才能证明它拦得住。
//!
//! `poise stop` 拿不到锁时改走常驻服务的本机接口，地址取服务启动时写在数据目录里的
//! `poise.listen`，不取启动文件里的 `listen`：服务可以用 `--listen` 换端口，按启动文件
//! 去连，可能连到同一端口上的别的服务。启动文件里的 `listen` 这里故意写成别的服务占着的 8000。

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use poise_runtime::InstanceLock;

/// 一个可以跑命令的实例目录：一份测试网启动文件，外加数据目录。
fn instance() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("poise.toml");
    std::fs::write(
        &path,
        "network = \"testnet\"\n\
         account_address = \"0x2222222222222222222222222222222222222222\"\n\
         api_wallet_private_key = \"0x0000000000000000000000000000000000000000000000000000000000000001\"\n\
         data_dir = \"data\"\n\
         listen = \"127.0.0.1:8000\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::create_dir(directory.path().join("data")).unwrap();
    directory
}

fn run(directory: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_poise"));
    command
        .arg("--instance")
        .arg(directory)
        .args(args)
        .output()
        .expect("the poise binary must be runnable")
}

#[test]
fn a_second_process_on_the_same_instance_refuses_to_start() {
    let directory = instance();
    let lock = InstanceLock::acquire(directory.path().join("data")).unwrap();

    // `serve` 取不到锁就什么也不做。
    let output = run(directory.path(), &["serve"]);
    assert!(!output.status.success(), "`poise serve` 应当拒绝运行");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already running"),
        "`poise serve` 的报错没有说明是被锁挡住：{stderr}"
    );

    // `stop` 取不到锁时不直接碰数据库。数据目录里没有服务写下的地址：它不去猜，
    // 不按启动文件里的 8000 去连，如实报出锁被占着、找不到服务地址。
    let output = run(directory.path(), &["stop"]);
    assert!(!output.status.success(), "找不到服务地址时应当失败");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("already running") && stderr.contains("poise.listen"),
        "`poise stop` 的报错要同时说明锁和服务地址：{stderr}"
    );

    // 服务写下了自己的地址：按它去连。这里那个端口没人监听，报接口连不上。
    std::fs::write(
        directory.path().join("data").join("poise.listen"),
        "127.0.0.1:1\n",
    )
    .unwrap();
    let output = run(directory.path(), &["stop"]);
    assert!(!output.status.success(), "连不上服务时应当失败");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("http://127.0.0.1:1"),
        "`poise stop` 应当说明连的是哪个地址：{stdout}"
    );
    assert!(
        stderr.contains("already running") && stderr.contains("cannot reach"),
        "`poise stop` 的报错要同时说明锁和接口：{stderr}"
    );

    // 放开锁之后同一个命令能跑起来：拒绝的原因确实是锁，不是别的。
    drop(lock);
    let output = run(directory.path(), &["stop"]);

    assert!(
        output.status.success(),
        "放开锁之后 `poise stop` 应当能跑：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("还没有策略"),
        "空实例上应当直接说还没有策略"
    );
}
