//! 常驻运行：把一个实例的 `serve` 交给 macOS 的 launchd。
//!
//! `install` 把当前的 `poise` 程序和网页控制台复制进实例目录（`bin/`、`console/`），写一份
//! 用户级 LaunchAgent，随登录启动、崩溃后自动拉起，并用 `caffeinate` 挡住系统睡眠。复制
//! 而不是直接指向仓库里的构建产物：仓库里重新编译、`cargo clean` 都不该悄悄换掉正在跑
//! 正式网的程序，换版本是再跑一次 `install`。
//!
//! 日志写在实例目录的 `logs/poise.log`（launchd 不轮转；info 级别的日志很少）。

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::instance::Instance;

/// LaunchAgent 的标签：一个实例一个。
fn label(directory: &Path) -> String {
    let name = directory
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "instance".to_string());
    let name: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("com.poise.{name}")
}

fn agent_path(label: &str) -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("cannot find the home directory")?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist")))
}

fn domain() -> Result<String> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("cannot run `id -u`")?;
    Ok(format!(
        "gui/{}",
        String::from_utf8_lossy(&output.stdout).trim()
    ))
}

/// 装上（或者换成当前版本）并启动。
pub fn install(
    instance: &Instance,
    directory: &Path,
    web_root: &Path,
    step_interval_secs: u64,
) -> Result<()> {
    let directory = directory
        .canonicalize()
        .with_context(|| format!("cannot resolve `{}`", directory.display()))?;
    if !web_root.join("index.html").is_file() {
        bail!(
            "`{}` 里没有 index.html：先在 console 里 `npm run build`",
            web_root.display()
        );
    }
    let label = label(&directory);
    let agent = agent_path(&label)?;
    let domain = domain()?;
    let console = directory.join("console");
    if web_root.canonicalize().ok() == console.canonicalize().ok() {
        bail!(
            "`--web-root` 指向的就是已经装好的 `{}`：要从仓库里的 console/dist 复制",
            console.display()
        );
    }

    // 文件全部准备好之后才停旧服务：中途失败时旧服务还在跑，不会变成没人驱动。程序是
    // 先写临时文件再改名，正在跑的旧进程不受影响。
    let bin = directory.join("bin");
    std::fs::create_dir_all(&bin)?;
    let program = bin.join("poise");
    let current = std::env::current_exe().context("cannot find the running program")?;
    // 先复制到临时文件再改名：直接覆盖正在执行的文件，macOS 会因为签名对不上杀掉进程。
    let staged = bin.join("poise.new");
    std::fs::copy(&current, &staged)
        .with_context(|| format!("cannot copy `{}`", current.display()))?;
    std::fs::rename(&staged, &program)?;

    // 控制台同样先复制到旁边，成功了再换上去。
    let staged_console = directory.join("console.new");
    if staged_console.exists() {
        std::fs::remove_dir_all(&staged_console)?;
    }
    copy_dir(web_root, &staged_console)?;
    if console.exists() {
        std::fs::remove_dir_all(&console)?;
    }
    std::fs::rename(&staged_console, &console)?;

    let logs = directory.join("logs");
    std::fs::create_dir_all(&logs)?;
    let log = logs.join("poise.log");

    let arguments = [
        "/usr/bin/caffeinate".to_string(),
        // 挡住空闲睡眠和系统睡眠（接电源时）；进程退出就失效。
        "-is".to_string(),
        program.display().to_string(),
        "--instance".to_string(),
        directory.display().to_string(),
        "serve".to_string(),
        "--step-interval-secs".to_string(),
        step_interval_secs.to_string(),
        "--web-root".to_string(),
        console.display().to_string(),
    ];
    std::fs::create_dir_all(agent.parent().expect("the agent path has a parent"))?;
    std::fs::write(&agent, plist(&label, &arguments, &directory, &log))
        .with_context(|| format!("cannot write `{}`", agent.display()))?;

    // 换上新版本：停掉旧的（没装过时这一步会失败，不要紧），再启动。
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{label}")])
        .output();

    // 旧的刚注销时 launchd 可能还没放手，bootstrap 会报 I/O 错误：等一等再试。
    let mut attempt = 0;
    loop {
        attempt += 1;
        let output = Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&agent)
            .output()
            .context("cannot run launchctl")?;
        if output.status.success() {
            break;
        }
        if attempt >= 10 {
            bail!(
                "launchctl bootstrap failed: {}（旧服务已经停了：修好之后再跑一次 install）",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
    println!("已装好 {label}（{}）", instance.label());
    println!("程序        {}", program.display());
    println!("控制台文件  {}", console.display());
    println!("日志        {}", log.display());
    println!("配置        {}", agent.display());
    println!("开机登录后自动启动，崩溃后自动拉起；停用：poise --instance … service uninstall");
    Ok(())
}

/// 停掉并删掉 LaunchAgent。复制进实例目录的程序和日志留着。
pub fn uninstall(directory: &Path) -> Result<()> {
    let directory = directory.canonicalize()?;
    let label = label(&directory);
    let agent = agent_path(&label)?;
    let output = Command::new("launchctl")
        .args(["bootout", &format!("{}/{label}", domain()?)])
        .output()
        .context("cannot run launchctl")?;
    if !output.status.success() {
        println!(
            "launchctl bootout：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if agent.exists() {
        std::fs::remove_file(&agent)?;
    }
    println!("已停用 {label}");
    Ok(())
}

/// launchd 眼里的状态：在不在跑、进程号、上次退出码。
pub fn status(directory: &Path) -> Result<()> {
    let directory = directory.canonicalize()?;
    let label = label(&directory);
    let output = Command::new("launchctl")
        .args(["print", &format!("{}/{label}", domain()?)])
        .output()
        .context("cannot run launchctl")?;
    if !output.status.success() {
        println!("{label} 没有装（或者没在 launchd 里）");
        return Ok(());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    println!("{label}");
    for line in text.lines().map(str::trim) {
        if line.starts_with("state =")
            || line.starts_with("pid =")
            || line.starts_with("last exit code =")
            || line.starts_with("runs =")
        {
            println!("  {line}");
        }
    }
    Ok(())
}

fn plist(label: &str, arguments: &[String], directory: &Path, log: &Path) -> String {
    let arguments: String = arguments
        .iter()
        .map(|argument| format!("        <string>{}</string>\n", escape(argument)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
{arguments}    </array>
    <key>WorkingDirectory</key>
    <string>{directory}</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
        label = escape(label),
        directory = escape(&directory.display().to_string()),
        log = escape(&log.display().to_string()),
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{label, plist};

    #[test]
    fn one_agent_per_instance() {
        assert_eq!(
            label(Path::new("/Users/me/.poise/mainnet")),
            "com.poise.mainnet"
        );
        assert_eq!(
            label(Path::new("/tmp/my instance")),
            "com.poise.my-instance"
        );
    }

    #[test]
    fn the_agent_keeps_the_service_alive_and_logs_into_the_instance() {
        let text = plist(
            "com.poise.testnet",
            &["/usr/bin/caffeinate".to_string(), "-is".to_string()],
            Path::new("/x/testnet"),
            Path::new("/x/testnet/logs/poise.log"),
        );
        assert!(text.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(text.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(text.contains("<string>/usr/bin/caffeinate</string>"));
        assert!(text.contains("<string>/x/testnet/logs/poise.log</string>"));
    }
}
