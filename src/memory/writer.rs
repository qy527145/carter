//! 记忆原子写 + 修订快照。
//!
//! 写流程：
//! 1. 如果目标文件已存在，先复制到 `~/.carter/memory_revisions/<filename>.<unix_ms>.bak`
//!    （只保留最近 10 个 revision，老的自动清理）
//! 2. 写入 tmp 文件 `<path>.tmp`，flush + fsync
//! 3. rename 替换目标文件（Unix 上原子，Windows 上覆盖）
//!
//! 崩溃恢复：rename 是原子的 → 进程死在第 2 步前，目标文件不变；死在第 3 步则要么是
//! 老版本要么是新版本，永远是完整的。

use std::io::Write;
use std::path::Path;

const REVISION_KEEP: usize = 10;

/// 原子写文件：先备份现有版本到 revisions 目录，再 tmp + rename。
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    // 0. 确保父目录存在。
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }

    // 1. 备份现有内容（如果有）到 revisions/。
    if path.exists() {
        let _ = snapshot_revision(path);
    }

    // 2. 写 tmp 文件。
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("bak")
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?; // 数据落盘后才能 rename
    }

    // 3. rename 替换目标。
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// 把当前 path 内容复制到 revisions/<filename>.<unix_ms>.bak；同时清理超出 KEEP 上限的老版。
/// 失败仅 warn 不传播（备份失败不应阻断主写入）。
fn snapshot_revision(path: &Path) -> std::io::Result<()> {
    let revs_dir = crate::config::paths::memory_revisions_dir();
    std::fs::create_dir_all(&revs_dir)?;

    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let stamp = crate::session::now_ms();
    let dest = revs_dir.join(format!("{filename}.{stamp}.bak"));
    std::fs::copy(path, &dest)?;

    // 清理：同前缀的 .bak 按 mtime 倒序，保留最近 KEEP 个。
    let _ = prune_revisions(&revs_dir, filename);
    Ok(())
}

fn prune_revisions(revs_dir: &Path, filename: &str) -> std::io::Result<()> {
    let prefix = format!("{filename}.");
    let mut entries: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();
    for entry in std::fs::read_dir(revs_dir)? {
        let e = entry?;
        let name = e.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(&prefix) && name_str.ends_with(".bak") {
            let mtime = e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            entries.push((e.path(), mtime));
        }
    }
    entries.sort_by(|a, b| b.1.cmp(&a.1)); // 最新在前
    for (path, _) in entries.into_iter().skip(REVISION_KEEP) {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 共享 CARTER_HOME 锁（在 config::paths::home_env_lock）—— media / memory 单测共用，
    /// 避免 cargo test 多线程下覆盖彼此的 env。
    fn home_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::paths::home_env_lock()
    }

    fn unique_home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("carter-mem-writer-{}", crate::session::now_ms()))
    }

    #[test]
    fn write_atomic_creates_parent_dirs() {
        let _g = home_lock();
        let tmp = unique_home();
        unsafe { std::env::set_var("CARTER_HOME", &tmp); }
        let target = tmp.join("nested").join("a").join("file.md");
        write_atomic(&target, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::remove_var("CARTER_HOME"); }
    }

    #[test]
    fn write_atomic_backs_up_existing_then_replaces() {
        let _g = home_lock();
        let tmp = unique_home();
        unsafe { std::env::set_var("CARTER_HOME", &tmp); }
        std::fs::create_dir_all(&tmp).unwrap();
        let target = tmp.join("CARTER.md");
        std::fs::write(&target, "v1").unwrap();

        write_atomic(&target, "v2").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "v2");

        // revisions 目录应至少有一个备份（先前测试可能也写过同名文件 — 用 ≥1）。
        let revs = crate::config::paths::memory_revisions_dir();
        let backups: Vec<_> = std::fs::read_dir(&revs)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("CARTER.md."))
            .collect();
        assert!(!backups.is_empty());
        // 备份内容是 v1（取最新一个）。
        let latest = backups
            .iter()
            .max_by_key(|e| e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        let backup_content = std::fs::read_to_string(latest.path()).unwrap();
        assert_eq!(backup_content, "v1");

        let _ = std::fs::remove_dir_all(&tmp);
        unsafe { std::env::remove_var("CARTER_HOME"); }
    }
}
