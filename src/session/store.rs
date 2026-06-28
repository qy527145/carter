//! Store —— 会话文件的布局、列举、加载、fork。
//! 布局：`~/.carter/projects/<project-key>/<created_ms>-<uuid>.jsonl`（见 docs/04）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::agent::Thread;
use crate::config::paths::carter_home;
use crate::provider::Message;
use crate::tools::TodoItem;

use super::{now_ms, GitInfo, Record, RecordKind, Recorder, SessionMeta};

/// 新会话的可选参数。
#[derive(Debug, Default)]
pub struct SessionOpts {
    /// 显式会话 id（`--session-id`）；缺省随机 uuid v4。
    pub session_id: Option<String>,
}

/// 列举条目：元数据 + 文件路径。
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub meta: SessionMeta,
    pub path: PathBuf,
}

/// `~/.carter/projects`。
fn projects_root() -> PathBuf {
    carter_home().join("projects")
}

/// cwd → 项目键：可读 basename + 路径哈希（避免跨盘/重名碰撞）。
pub fn project_key(cwd: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    cwd.to_string_lossy().hash(&mut h);
    let hash = h.finish();
    let base = cwd
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());
    let base: String = base
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    format!("{base}-{hash:016x}")
}

/// 某 cwd 的会话目录。
pub fn project_dir(cwd: &Path) -> PathBuf {
    projects_root().join(project_key(cwd))
}

/// 读 git 快照（best-effort，非 git 仓库返回 None）。
fn read_git(cwd: &Path) -> Option<GitInfo> {
    let run = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let commit = run(&["rev-parse", "--short", "HEAD"])?;
    let branch = run(&["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    Some(GitInfo { commit, branch })
}

fn session_filename(created_at: u64, id: &str) -> String {
    format!("{created_at:013}-{id}.jsonl")
}

/// 开新会话：建 meta + recorder，写首行 session_meta，返回空 Thread（已挂 recorder）。
pub fn start_new(
    cwd: &Path,
    model: &str,
    opts: &SessionOpts,
) -> crate::Result<(Thread, SessionMeta)> {
    let id = opts
        .session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let created_at = now_ms();
    let meta = SessionMeta {
        id: id.clone(),
        parent_id: None,
        forked_from: None,
        cwd: cwd.to_string_lossy().to_string(),
        git: read_git(cwd),
        title: None,
        carter_version: env!("CARGO_PKG_VERSION").to_string(),
        model: model.to_string(),
        created_at,
    };
    let path = project_dir(cwd).join(session_filename(created_at, &id));
    // 惰性建文件：直到首条真实记录（首条 user 消息）才落盘，避免空会话堆积。
    let header = Record::new(RecordKind::SessionMeta(meta.clone()));
    let recorder = Arc::new(Recorder::deferred(&path, &header));
    let mut thread = Thread::new_empty();
    thread.set_recorder(recorder);
    Ok((thread, meta))
}

/// 折叠一个会话文件 → (messages, todos, meta)。Compacted 整体替换 messages。
fn fold_file(path: &Path) -> crate::Result<(Vec<Message>, Vec<TodoItem>, SessionMeta)> {
    let raw = std::fs::read_to_string(path)?;
    let mut messages: Vec<Message> = Vec::new();
    let mut todos: Vec<TodoItem> = Vec::new();
    let mut meta: Option<SessionMeta> = None;
    let mut title: Option<String> = None;
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let rec: Record = match serde_json::from_str(line) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("session: skip malformed line in {}: {e}", path.display());
                continue;
            }
        };
        match rec.kind {
            RecordKind::SessionMeta(m) => meta = Some(m),
            RecordKind::Title { title: t } => title = Some(t),
            RecordKind::Message(m) => messages.push(m),
            RecordKind::Todo(t) => todos = t,
            RecordKind::Compacted { messages: snap, .. } => messages = snap,
        }
    }
    let mut meta = meta.ok_or_else(|| {
        crate::error::CarterError::Config(format!("session file missing meta: {}", path.display()))
    })?;
    // title 记录晚于 meta，回填。
    if title.is_some() {
        meta.title = title;
    }
    Ok((messages, todos, meta))
}

/// 加载会话续接（追加写同一文件）。
pub fn load(entry: &SessionEntry) -> crate::Result<(Thread, SessionMeta)> {
    let (messages, todos, meta) = fold_file(&entry.path)?;
    let recorder = Arc::new(Recorder::open(&entry.path)?);
    Ok((Thread::from_parts(messages, todos, recorder), meta))
}

/// fork：种子=父历史，写到全新自包含文件（含 forked_from 血缘）。
pub fn fork(entry: &SessionEntry) -> crate::Result<(Thread, SessionMeta)> {
    let (messages, todos, parent) = fold_file(&entry.path)?;
    let id = uuid::Uuid::new_v4().to_string();
    let created_at = now_ms();
    let cwd = PathBuf::from(&parent.cwd);
    let meta = SessionMeta {
        id: id.clone(),
        parent_id: None,
        forked_from: Some(parent.id.clone()),
        cwd: parent.cwd.clone(),
        git: read_git(&cwd),
        title: parent.title.clone(),
        carter_version: env!("CARGO_PKG_VERSION").to_string(),
        model: parent.model.clone(),
        created_at,
    };
    let path = project_dir(&cwd).join(session_filename(created_at, &id));
    let recorder = Arc::new(Recorder::open(&path)?);
    recorder.record(RecordKind::SessionMeta(meta.clone()));
    if let Some(t) = &meta.title {
        recorder.record(RecordKind::Title { title: t.clone() });
    }
    // 把种子历史 + todo 写入新文件，使其自包含。
    for m in &messages {
        recorder.record(RecordKind::Message(super::cap_for_persist(m)));
    }
    if !todos.is_empty() {
        recorder.record(RecordKind::Todo(todos.clone()));
    }
    Ok((Thread::from_parts(messages, todos, recorder), meta))
}

/// 列举会话。`all=false` 仅当前 cwd；`all=true` 跨所有项目。按创建时间倒序。
pub fn list(cwd: &Path, all: bool) -> Vec<SessionEntry> {
    let pattern = if all {
        projects_root().join("*").join("*.jsonl")
    } else {
        project_dir(cwd).join("*.jsonl")
    };
    let mut entries: Vec<SessionEntry> = Vec::new();
    let glob_pat = pattern.to_string_lossy().to_string();
    if let Ok(paths) = glob::glob(&glob_pat) {
        for p in paths.flatten() {
            if let Ok((_, _, meta)) = fold_file(&p) {
                entries.push(SessionEntry { meta, path: p });
            }
        }
    }
    entries.sort_by(|a, b| b.meta.created_at.cmp(&a.meta.created_at));
    entries
}

/// 当前 cwd 最近一条会话（`--continue`）。
pub fn latest(cwd: &Path) -> Option<SessionEntry> {
    list(cwd, false).into_iter().next()
}

/// 压实一个会话文件：折叠到当前（压缩后）状态，重写为
/// `session_meta` + 一条 `Compacted` 全量快照 + （可选）`todo`。
/// 物理丢弃被 `Compacted` 覆盖的原始工具输出 / 历史快照，resume 行为不变。
/// 返回 (旧字节数, 新字节数)。
pub fn gc(entry: &SessionEntry) -> crate::Result<(u64, u64)> {
    let old = std::fs::metadata(&entry.path).map(|m| m.len()).unwrap_or(0);
    let (messages, todos, meta) = fold_file(&entry.path)?;

    let mut buf = String::new();
    let mut push = |kind: RecordKind| {
        if let Ok(s) = serde_json::to_string(&Record::new(kind)) {
            buf.push_str(&s);
            buf.push('\n');
        }
    };
    // meta 已被 fold_file 回填 title，单条即可（无需再写 Title 行）。
    push(RecordKind::SessionMeta(meta));
    if !messages.is_empty() {
        push(RecordKind::Compacted {
            tier: "gc".into(),
            messages,
        });
    }
    if !todos.is_empty() {
        push(RecordKind::Todo(todos));
    }

    // 写临时文件再原子替换（Windows 上 rename 会覆盖目标）。
    let tmp = entry.path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, &buf)?;
    std::fs::rename(&tmp, &entry.path)?;
    Ok((old, buf.len() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("carter-test-{tag}-{}.jsonl", now_ms()))
    }

    fn write_lines(path: &Path, recs: &[RecordKind]) {
        let rec = Recorder::open(path).unwrap();
        for k in recs {
            rec.record(k.clone());
        }
    }

    fn meta() -> SessionMeta {
        SessionMeta {
            id: "s1".into(),
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
    fn fold_accumulates_messages_and_backfills_title() {
        let path = temp_file("fold");
        write_lines(
            &path,
            &[
                RecordKind::SessionMeta(meta()),
                RecordKind::Message(Message::User("q".into())),
                RecordKind::Message(Message::Assistant("a".into())),
                RecordKind::Title { title: "标题".into() },
            ],
        );
        let (messages, _todos, m) = fold_file(&path).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(m.title.as_deref(), Some("标题"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn compacted_snapshot_replaces_prior_messages() {
        let path = temp_file("compact");
        write_lines(
            &path,
            &[
                RecordKind::SessionMeta(meta()),
                RecordKind::Message(Message::User("q1".into())),
                RecordKind::Message(Message::Assistant("a1".into())),
                RecordKind::Compacted {
                    tier: "L3".into(),
                    messages: vec![Message::Assistant("【摘要】".into())],
                },
                RecordKind::Message(Message::User("q2".into())),
            ],
        );
        let (messages, _todos, _m) = fold_file(&path).unwrap();
        // 压缩快照整体替换前序，再叠加其后的新消息。
        assert_eq!(messages.len(), 2);
        assert!(matches!(&messages[0], Message::Assistant(s) if s == "【摘要】"));
        assert!(matches!(&messages[1], Message::User(s) if s == "q2"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn project_key_is_stable_and_sanitized() {
        let k1 = project_key(Path::new("/home/u/proj"));
        let k2 = project_key(Path::new("/home/u/proj"));
        assert_eq!(k1, k2);
        assert!(k1.starts_with("proj-"));
        assert!(!project_key(Path::new("/a/b c")).contains(' '));
    }

    #[test]
    fn gc_collapses_history_but_preserves_replay_state() {
        let path = temp_file("gc");
        write_lines(
            &path,
            &[
                RecordKind::SessionMeta(meta()),
                RecordKind::Title { title: "标题".into() },
                RecordKind::Message(Message::User("q1".into())),
                RecordKind::Message(Message::Tool {
                    call_id: "1".into(),
                    content: "x".repeat(5000), // 大块原始工具输出
                }),
                RecordKind::Message(Message::Assistant("a1".into())),
                RecordKind::Message(Message::User("q2".into())),
            ],
        );
        let entry = SessionEntry {
            meta: meta(),
            path: path.clone(),
        };
        let before = fold_file(&path).unwrap().0;
        let (old, new) = gc(&entry).unwrap();
        assert!(new < old, "gc 应缩小文件: {old} -> {new}");
        // 重放状态不变（消息序列一致），标题保留。
        let (after, _todos, m) = fold_file(&path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(m.title.as_deref(), Some("标题"));
        let _ = std::fs::remove_file(&path);
    }
}
