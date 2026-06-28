//! Recorder —— 把 `Record` 追加写到会话 jsonl。
//! 失败仅 warn 不中断主循环（错误即数据）。Unix 下文件 0600。
//!
//! 惰性建文件：新会话用 [`Recorder::deferred`] 缓存首行（session_meta），
//! 直到**第一条真实记录**到来才创建文件并先写首行——避免「开了就关」的空会话堆积。
//! resume/fork 走 [`Recorder::open`]，立即建/打开（已有内容要写）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{Record, RecordKind};

#[derive(Debug)]
struct Inner {
    /// None 表示文件尚未创建（惰性）。
    file: Option<File>,
    /// 待写首行（session_meta 序列化）；建文件时先落它。写出后置 None。
    deferred_header: Option<String>,
}

/// 单会话录制器。内部持有追加写句柄（Mutex 串行化多处写入）。
#[derive(Debug)]
pub struct Recorder {
    /// 会话文件路径（保留供诊断 / 将来 gc）。
    #[allow(dead_code)]
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl Recorder {
    /// 立即打开（或创建）文件用于追加（resume/fork）。Unix 下设 0600。
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let file = create_append(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner {
                file: Some(file),
                deferred_header: None,
            }),
        })
    }

    /// 惰性录制器：缓存首行，直到第一条记录才真正建文件（新会话）。
    pub fn deferred(path: &Path, header: &Record) -> Self {
        let header_line = serde_json::to_string(header).ok();
        Self {
            path: path.to_path_buf(),
            inner: Mutex::new(Inner {
                file: None,
                deferred_header: header_line,
            }),
        }
    }

    /// 会话文件路径（保留供诊断 / 将来 gc）。
    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 序列化一行并写入 + flush。失败 warn 不传播。
    pub fn record(&self, kind: RecordKind) {
        let rec = Record::new(kind);
        let line = match serde_json::to_string(&rec) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("session: serialize record failed: {e}");
                return;
            }
        };
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        // 惰性建文件：首次写入时创建并先落缓存的首行。
        if inner.file.is_none() {
            match create_append(&self.path) {
                Ok(f) => inner.file = Some(f),
                Err(e) => {
                    tracing::warn!("session: create file failed: {e}");
                    return;
                }
            }
            if let Some(header) = inner.deferred_header.take() {
                if let Some(f) = inner.file.as_mut() {
                    let _ = writeln!(f, "{header}");
                }
            }
        }
        if let Some(f) = inner.file.as_mut() {
            if let Err(e) = writeln!(f, "{line}").and_then(|_| f.flush()) {
                tracing::warn!("session: write record failed: {e}");
            }
        }
    }
}

/// 创建（若需）父目录并以追加模式打开文件；Unix 下设 0600。
fn create_append(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Message;
    use crate::session::{now_ms, SessionMeta};

    fn meta() -> SessionMeta {
        SessionMeta {
            id: "t".into(),
            parent_id: None,
            forked_from: None,
            cwd: "/x".into(),
            git: None,
            title: None,
            carter_version: "0.1.0".into(),
            model: "m".into(),
            created_at: 1,
        }
    }

    #[test]
    fn deferred_creates_file_only_on_first_record() {
        let path = std::env::temp_dir().join(format!("carter-rec-{}.jsonl", now_ms()));
        let _ = std::fs::remove_file(&path);
        let header = Record::new(RecordKind::SessionMeta(meta()));
        let rec = Recorder::deferred(&path, &header);
        // 尚未写入任何记录 → 文件不存在（空会话不落盘）。
        assert!(!path.exists());

        rec.record(RecordKind::Message(Message::User("hi".into())));
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        // 首行是缓存的 session_meta，其后才是消息。
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("\"type\":\"session_meta\""));
        assert!(lines[1].contains("\"type\":\"message\""));
        let _ = std::fs::remove_file(&path);
    }
}

