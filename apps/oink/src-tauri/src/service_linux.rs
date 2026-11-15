//! Linux 后台自动拉起：`oink` 启动时若发现 agent 未运行，就在同目录找到
//! `oink-agent` 并以 `run` 子命令后台启动，等 IPC 握手成功后继续。
//!
//! 与 Windows 的差异：Windows 由 SYSTEM 服务经 UIAccess 拉起、受签名/权限约束；
//! Linux 没有这层，普通用户进程直接 spawn 即可。语义保持一致——
//! 只在启动时尝试一次，UI 生命周期内不重启崩溃的后台。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use oink_agent::remote::RemoteApi;

/// 冷启动预算：agent 要建身份、绑 QUIC、发布 IPC 端点，首启约 1–3 秒。
const START_TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(100);

/// 与 UI 可执行文件同目录的 `oink-agent`。
fn sibling_agent() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let directory = exe.parent()?;
    let candidate = directory.join("oink-agent");
    candidate.is_file().then_some(candidate)
}

/// PATH 里的 `oink-agent`（deb/pacman 安装后都在 /usr/bin）。
fn path_agent() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("oink-agent"))
        .find(|candidate| candidate.is_file())
}

fn agent_binary() -> Option<PathBuf> {
    sibling_agent().or_else(path_agent)
}

/// 确保后台在运行并完成握手。返回 Ok 表示可以继续创建界面。
///
/// 三种情形：
/// - 已在运行：只做 hello 校验；
/// - 未运行：拉起 `oink-agent run`（脱离 UI 进程，stdio 接 /dev/null），轮询等就绪；
/// - 找不到二进制：明确报错，提示如何安装/启动。
pub fn ensure_backend(remote: &RemoteApi, data_dir: &Path) -> Result<(), String> {
    match oink_agent::instance::is_running(data_dir) {
        Some(true) => {
            remote.reset_connections();
            return remote.hello().map(|_| ()).map_err(|e| format!("backend unavailable: {e}"));
        }
        None => return Err("backend_ownership_unavailable".into()),
        Some(false) => {}
    }

    let binary = agent_binary().ok_or_else(|| {
        "找不到 oink-agent：请安装 Oink 包（含 oink-agent），或把它放到 oink 同目录/PATH".to_string()
    })?;
    tracing::info!(agent = %binary.display(), "backend not running; starting it");

    let mut command = Command::new(&binary);
    command.arg("run");
    // 数据目录经环境传递，保证 UI 与后台看同一个 home（OINK_HOME 已设置时继承）。
    oink_agent::backend::launch::spawn_detached(&mut command)
        .map_err(|e| format!("failed to start {}: {e}", binary.display()))?;

    let deadline = Instant::now() + START_TIMEOUT;
    let mut last_error = String::new();
    while Instant::now() < deadline {
        std::thread::sleep(POLL);
        match oink_agent::instance::is_running(data_dir) {
            None => return Err("backend_ownership_unavailable".into()),
            Some(false) => continue,
            Some(true) => {
                remote.reset_connections();
                match remote.hello_with_timeout(Duration::from_millis(500)) {
                    Ok(_) => {
                        tracing::info!("backend started and handshook");
                        return Ok(());
                    }
                    Err(error) => last_error = error.to_string(),
                }
            }
        }
    }
    Err(if last_error.is_empty() {
        format!("后台启动超时（{}s）：查看日志 ~/.local/state/oink/Logs", START_TIMEOUT.as_secs())
    } else {
        format!("后台启动后握手失败：{last_error}")
    })
}

/// 换个 stderr 便于诊断：把 agent 输出落到数据目录的 agent.log。
#[allow(dead_code)]
pub fn spawn_with_log(binary: &Path, log: &Path) -> std::io::Result<std::process::Child> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    Command::new(binary)
        .arg("run")
        .stdin(Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file)
        .spawn()
}
