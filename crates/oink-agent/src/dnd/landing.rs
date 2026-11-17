//! Linux landing: Wayland cannot originate a native drag (see
//! `docs/linux.md`), so a peer's incoming file drag is saved directly into the
//! XDG download directory instead of being placed onto a native drop target.
//! The saved paths are then published to the system clipboard so the user can
//! paste them into an application.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// XDG download directory (`$XDG_DOWNLOAD_DIR`, else `~/Downloads`).
fn download_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DOWNLOAD_DIR") {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Downloads"))
}

/// Reject absolute paths, `..`, and empty components so a peer cannot escape
/// the download directory.
fn safe_relative(path: &str) -> Option<PathBuf> {
    // 绝对路径与含 `..` 的路径一律拒绝：不能把对端的文件名当成本机路径。
    if path.starts_with('/') || path.contains('\\') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in path.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            other => out.push(other),
        }
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

/// Copy every file of `contents` into the download directory, creating parent
/// folders as needed. Returns the saved top-level absolute paths.
pub(crate) fn land(
    contents: Arc<dyn oink_clipboard::FileContents>,
    files: &[oink_foundation::clipboard::FileEntry],
) -> Result<Vec<PathBuf>, String> {
    let root = download_dir().ok_or_else(|| "no download directory".to_string())?;
    std::fs::create_dir_all(&root).map_err(|e| format!("create {}: {e}", root.display()))?;
    let mut saved: Vec<PathBuf> = Vec::new();
    for (index, entry) in files.iter().enumerate() {
        let Some(relative) = safe_relative(&entry.path) else {
            tracing::warn!(path = %entry.path, "dnd landing: unsafe relative path skipped");
            continue;
        };
        let target = root.join(&relative);
        if entry.is_dir {
            std::fs::create_dir_all(&target).map_err(|e| format!("create {}: {e}", target.display()))?;
            continue;
        }
        if let Some(parent) = target.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return Err(format!("create {}: {e}", parent.display()));
        }
        let mut reader = contents
            .open(index)
            .map_err(|e| format!("open {}: {e}", entry.path))?;
        let mut data = Vec::new();
        reader
            .read_to_end(&mut data)
            .map_err(|e| format!("read {}: {e}", entry.path))?;
        // A landing file never overwrites an unrelated file silently: pick a
        // unique name when the target already exists.
        let target = unique_path(&target);
        let mut out = std::fs::File::create(&target).map_err(|e| format!("create {}: {e}", target.display()))?;
        out.write_all(&data)
            .map_err(|e| format!("write {}: {e}", target.display()))?;
        if !saved.contains(&target) {
            // Only top-level entries are pasted; children of a saved folder are
            // reachable through their parent path.
            let top = root.join(relative.iter().next().map(Path::new).unwrap_or(Path::new("")));
            if !saved.contains(&top) {
                saved.push(top);
            }
        }
    }
    Ok(saved)
}

/// `name.ext` → `name (1).ext` when the path is taken.
fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let parent = path.parent().unwrap_or(Path::new(""));
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 1..10_000 {
        let name = match ext {
            Some(ext) => format!("{stem} ({n}).{ext}"),
            None => format!("{stem} ({n})"),
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf()
}

/// Publish saved paths as `text/uri-list`-compatible text on the system
/// clipboard, so the user can paste them into a file manager or terminal.
pub(crate) fn publish_paths(paths: &[PathBuf]) {
    if paths.is_empty() {
        return;
    }
    let text = paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let mut payload = std::collections::HashMap::new();
    payload.insert("text/plain;charset=utf-8".to_string(), text.clone().into_bytes());
    payload.insert("text/plain".to_string(), text.clone().into_bytes());
    payload.insert("UTF8_STRING".to_string(), text.clone().into_bytes());
    payload.insert("STRING".to_string(), text.clone().into_bytes());
    payload.insert("TEXT".to_string(), text.into_bytes());
    if let Err(error) = oink_clipboard::write_raw_payload(payload) {
        tracing::warn!(%error, "dnd landing: clipboard publish failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_paths() {
        assert!(safe_relative("a/b.txt").is_some());
        assert!(safe_relative("../etc/passwd").is_none());
        assert!(safe_relative("/etc/passwd").is_none());
        assert!(safe_relative("").is_none());
        assert!(safe_relative("a/../../b").is_none());
    }

    #[test]
    fn unique_path_appends_suffix() {
        let dir = std::env::temp_dir().join(format!("oink-land-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("x.txt");
        std::fs::write(&a, b"1").unwrap();
        let b = unique_path(&a);
        assert_ne!(a, b);
        assert!(b.to_string_lossy().contains("(1)"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
