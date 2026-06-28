//! 跨会话输入历史 `~/.carter/history.jsonl`（仿 Codex）。
//! 一行一条 `{ts, session_id, text}`：记录用户提交的原始输入，供上下方向键跨会话召回。
//! 与 per-session rollout（projects/…/<id>.jsonl）是两套：这里只是「你打过什么」的轻量列表。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::paths::carter_home;

use super::now_ms;

/// 启动时载入的最近条数上限（避免历史过大拖慢启动 / 占内存）。
pub const LOAD_LIMIT: usize = 1000;

fn history_path() -> PathBuf {
    carter_home().join("history.jsonl")
}

#[derive(Debug, Serialize, Deserialize)]
struct HistoryEntry {
    ts: u64,
    session_id: String,
    text: String,
}

/// 追加一条输入历史。best-effort：失败仅 warn 不中断。
pub fn append(session_id: &str, text: &str) {
    append_to(&history_path(), session_id, text);
}

/// 载入最近 `LOAD_LIMIT` 条输入文本（按时间升序，最新在末尾，契合 TUI 的 `sent`）。
pub fn load() -> Vec<String> {
    load_from(&history_path(), LOAD_LIMIT)
}

fn append_to(path: &Path, session_id: &str, text: &str) {
    let entry = HistoryEntry {
        ts: now_ms(),
        session_id: session_id.to_string(),
        text: text.to_string(),
    };
    let line = match serde_json::to_string(&entry) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("history: serialize failed: {e}");
            return;
        }
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
            }
            if let Err(e) = writeln!(f, "{line}") {
                tracing::warn!("history: write failed: {e}");
            }
        }
        Err(e) => tracing::warn!("history: open failed: {e}"),
    }
}

fn load_from(path: &Path, limit: usize) -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut texts: Vec<String> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<HistoryEntry>(l).ok())
        .map(|e| e.text)
        .collect();
    if texts.len() > limit {
        texts = texts.split_off(texts.len() - limit);
    }
    texts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("carter-hist-{tag}-{}.jsonl", now_ms()))
    }

    #[test]
    fn append_then_load_roundtrip_in_order() {
        let path = temp("roundtrip");
        let _ = std::fs::remove_file(&path);
        append_to(&path, "s1", "第一条");
        append_to(&path, "s1", "第二条");
        let got = load_from(&path, 10);
        assert_eq!(got, vec!["第一条".to_string(), "第二条".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_caps_to_limit_keeping_newest() {
        let path = temp("cap");
        let _ = std::fs::remove_file(&path);
        for i in 0..5 {
            append_to(&path, "s", &format!("m{i}"));
        }
        // 限 2 → 保留最新 2 条（m3, m4）。
        let got = load_from(&path, 2);
        assert_eq!(got, vec!["m3".to_string(), "m4".to_string()]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_file_is_empty() {
        assert!(load_from(Path::new("/no/such/carter-hist.jsonl"), 10).is_empty());
    }

    #[test]
    fn skips_malformed_lines() {
        let path = temp("malformed");
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "not json\n{\"ts\":1,\"session_id\":\"s\",\"text\":\"ok\"}\n").unwrap();
        assert_eq!(load_from(&path, 10), vec!["ok".to_string()]);
        let _ = std::fs::remove_file(&path);
    }
}
