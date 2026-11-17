//! Linux: XDG autostart (`~/.config/autostart/oink.desktop`).
//!
//! 登录时由桌面环境（niri 等遵循 freedesktop autostart-spec 的会话）按 desktop
//! 文件启动主程序。登记的是应用本身，登录时像用户双击一样打开；desktop 的
//! Exec 带 `OINK_LAUNCHED_AT_LOGIN=1`，使本次启动不弹主窗口（见
//! [`crate::launched_at_login`]）。状态以系统（desktop 文件）为准，用户在系统
//! 设置或手工删改该文件后 [`status`] 能看到。
//!
//! Linux 无机器级「登录前」部件（无 Windows 那种 UIAccess 服务），用
//! [`crate::NoMachineLayer`]。

use std::fs;
use std::path::{Path, PathBuf};

use crate::{LaunchAtLoginProblem, LaunchAtLoginState, LaunchAtLoginStatus, LOGIN_LAUNCH_ENV};

const DESKTOP_NAME: &str = "oink.desktop";

/// `$XDG_CONFIG_HOME/autostart/oink.desktop`；XDG_CONFIG_HOME 未设时 `~/.config/autostart/`。
fn desktop_path() -> Result<PathBuf, LaunchAtLoginProblem> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .ok_or(LaunchAtLoginProblem::SystemUnavailable)?;
    Ok(config.join("autostart").join(DESKTOP_NAME))
}

/// 供登录项 Exec 使用的程序路径。
/// - AppImage：AppImage runtime 把 `$APPIMAGE` 设为 AppImage 文件本身，重启后仍有效；
///   `current_exe()` 指向挂载点内的临时路径，不能写进 desktop。
/// - 常规安装（deb/pacman）：PATH 里是 `/usr/bin/oink`，写命令名即可，避免绝对路径失效。
/// - 裸二进制（cargo run 等）：无稳定安装身份，拒绝登记。
fn exec_command() -> Result<String, LaunchAtLoginProblem> {
    if let Some(appimage) = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty()) {
        return Ok(format!("env {LOGIN_LAUNCH_ENV}=1 {}", PathBuf::from(appimage).display()));
    }
    // PATH 中能找到 oink 才认为是从安装形态运行。
    let on_path = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            let candidate = dir.join("oink");
            candidate.is_file()
        })
    });
    if on_path {
        Ok(format!("env {LOGIN_LAUNCH_ENV}=1 oink"))
    } else {
        Err(LaunchAtLoginProblem::InstalledAppRequired)
    }
}

fn desktop_contents() -> Result<String, LaunchAtLoginProblem> {
    Ok(format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Oink\n\
         Comment=Oink — your computers, working as one\n\
         Exec={}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        exec_command()?
    ))
}

/// 从现有 desktop 文件判断状态。
fn status_from(path: &Path) -> LaunchAtLoginStatus {
    let Ok(contents) = fs::read_to_string(path) else {
        return LaunchAtLoginStatus::new(LaunchAtLoginState::Disabled, None);
    };
    if !contents.contains("[Desktop Entry]") || !contents.contains("Exec=") {
        return LaunchAtLoginStatus::new(LaunchAtLoginState::Disabled, None);
    }
    let hidden = contents.lines().any(|line| {
        let line = line.trim();
        line.eq_ignore_ascii_case("Hidden=true")
    });
    let gnome_disabled = contents.lines().any(|line| {
        let line = line.trim();
        line.eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
    });
    if hidden || gnome_disabled {
        LaunchAtLoginStatus::new(LaunchAtLoginState::Disabled, None)
    } else {
        LaunchAtLoginStatus::new(LaunchAtLoginState::Enabled, None)
    }
}

pub(crate) fn status() -> LaunchAtLoginStatus {
    // 与 macOS bundle_problem 对齐：非安装形态（PATH 里没有 oink 且非 AppImage）
    // 报 Unavailable，避免界面显示一个点了必然失败的开关。
    if let Err(problem) = exec_command() {
        return LaunchAtLoginStatus::unavailable(problem);
    }
    match desktop_path() {
        Ok(path) => status_from(&path),
        Err(problem) => LaunchAtLoginStatus::unavailable(problem),
    }
}

pub(crate) fn set_enabled(enabled: bool) -> Result<LaunchAtLoginStatus, String> {
    let path = desktop_path().map_err(|p| p.code().to_string())?;
    if enabled {
        let contents = desktop_contents().map_err(|p| p.code().to_string())?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                tracing::warn!(error = %e, "create autostart dir failed");
                LaunchAtLoginProblem::SystemUnavailable.code().to_string()
            })?;
        }
        fs::write(&path, contents).map_err(|e| {
            tracing::warn!(error = %e, "write autostart desktop failed");
            LaunchAtLoginProblem::SystemUnavailable.code().to_string()
        })?;
    } else if path.exists() {
        fs::remove_file(&path).map_err(|e| {
            tracing::warn!(error = %e, "remove autostart desktop failed");
            LaunchAtLoginProblem::SystemUnavailable.code().to_string()
        })?;
    }
    Ok(status_from(&path))
}

pub(crate) fn open_system_settings() -> Result<(), String> {
    // Linux 无统一的登录项设置页；打开 autostart 目录让用户自行管理。
    let dir = desktop_path()
        .map_err(|p| p.code().to_string())?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| LaunchAtLoginProblem::SystemUnavailable.code().to_string())?;
    let _ = fs::create_dir_all(&dir);
    std::process::Command::new("xdg-open")
        .arg(&dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            tracing::warn!(error = %e, "xdg-open autostart dir failed");
            "system_unavailable".into()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_recognizes_enabled_disabled_and_foreign_files() {
        let dir = std::env::temp_dir().join(format!("oink-autostart-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(DESKTOP_NAME);

        fs::write(&path, "[Desktop Entry]\nExec=env OINK_LAUNCHED_AT_LOGIN=1 oink\n").unwrap();
        assert_eq!(status_from(&path).state, LaunchAtLoginState::Enabled);

        fs::write(&path, "[Desktop Entry]\nExec=oink\nHidden=true\n").unwrap();
        assert_eq!(status_from(&path).state, LaunchAtLoginState::Disabled);

        fs::write(&path, "not a desktop file\n").unwrap();
        assert_eq!(status_from(&path).state, LaunchAtLoginState::Disabled);

        assert_eq!(status_from(&dir.join("missing.desktop")).state, LaunchAtLoginState::Disabled);
        fs::remove_dir_all(&dir).ok();
    }
}
