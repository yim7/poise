//! 一个实例只允许一个写进程：第二个进程拿不到实例锁就拒绝运行。
//!
//! 这里起的是真的第二个进程（`poise` 二进制），不是同一进程里的第二个句柄：
//! 文件锁是进程级的，只有真起一个进程才能证明它拦得住。

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

    // 会写数据库、会下单撤单的命令都要先取锁，取不到就什么也不做。
    for args in [&["grid", "--once"][..], &["stop"][..]] {
        let output = run(directory.path(), args);

        assert!(
            !output.status.success(),
            "`poise {}` 应当拒绝运行",
            args.join(" ")
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("already running"),
            "`poise {}` 的报错没有说明是被锁挡住：{stderr}",
            args.join(" ")
        );
    }

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
