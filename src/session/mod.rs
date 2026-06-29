//! Session —— 会话历史持久化与续接。设计见 docs/04-session-management.md。
//!
//! 数据面：磁盘记录压缩前全量（追加写 JSONL），内存压缩只动 `Thread.messages`。
//! 落盘扼流点是 `Thread::append`/`append_user`/`set_todos`，压缩另写 `Compacted` 快照。

mod recorder;
mod store;

pub mod history;

pub use recorder::Recorder;
pub use store::{fork, gc, latest, list, load, start_new, SessionEntry, SessionOpts};

use serde::{Deserialize, Serialize};

use crate::provider::Message;
use crate::tools::TodoItem;

/// epoch 毫秒（无需引入日期 crate；文件名前缀用它保证可排序）。
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 自 1970-01-01 的天数 → (年, 月, 日)。Howard Hinnant 的 civil_from_days 算法。
/// 无日期 crate 依赖；llm_log 的时间戳与 prompt 的「今日」共用此函数。
pub(crate) fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

/// epoch ms → `YYYY-MM-DD`（UTC）。供 system prompt 的运行环境段使用。
pub fn date_utc(ms: u64) -> String {
    let days = ((ms / 1000) as i64).div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// 会话元数据（jsonl 首行 `session_meta`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    /// resume 的来源（同一文件追加时为空；保留给将来语义）。
    #[serde(default)]
    pub parent_id: Option<String>,
    /// fork 的来源会话 id。
    #[serde(default)]
    pub forked_from: Option<String>,
    pub cwd: String,
    #[serde(default)]
    pub git: Option<GitInfo>,
    #[serde(default)]
    pub title: Option<String>,
    pub carter_version: String,
    pub model: String,
    /// 创建时间（epoch ms）。
    pub created_at: u64,
}

/// 会话创建时的 git 快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitInfo {
    pub commit: String,
    pub branch: String,
}

/// 一行记录（`{ts, type, payload}`，type 标签 + payload 内容）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum RecordKind {
    SessionMeta(SessionMeta),
    Title { title: String },
    Message(Message),
    Todo(Vec<TodoItem>),
    /// 压缩后的全量快照（head + 摘要 + recent），等价 Codex 的 replacement_history。
    Compacted { tier: String, messages: Vec<Message> },
    /// 文件检查点：write_file/edit_file 等工具执行前的快照（供 /rewind）。
    /// 跨会话持久化：resume 时 fold_file 把这些恢复进 thread.checkpoints。
    Checkpoint {
        label: String,
        snapshots: Vec<FileSnapshotRecord>,
    },
}

/// FileSnapshot 的序列化形态（避开 PathBuf 直接序列化的歧义）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshotRecord {
    pub path: String,
    /// None = 当时文件不存在（回滚即删除）。
    pub prior: Option<String>,
}

/// 落盘的一行（带时间戳）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub ts: u64,
    #[serde(flatten)]
    pub kind: RecordKind,
}

impl Record {
    pub fn new(kind: RecordKind) -> Self {
        Self { ts: now_ms(), kind }
    }
}

/// 单条 `Message::Tool` 落盘上限（治膨胀，对应 docs 原则 4）。超出截断 + 标记。
pub const TOOL_OUTPUT_PERSIST_CAP: usize = 32 * 1024;

/// 落盘前对消息做体积约束：仅截断超大的工具输出，其余原样。
pub fn cap_for_persist(msg: &Message) -> Message {
    match msg {
        Message::Tool { call_id, content } if content.len() > TOOL_OUTPUT_PERSIST_CAP => {
            let kept: String = content.chars().take(TOOL_OUTPUT_PERSIST_CAP).collect();
            Message::Tool {
                call_id: call_id.clone(),
                content: format!(
                    "{kept}\n[tool result truncated for session log: {} bytes total]",
                    content.len()
                ),
            }
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每行落盘格式 = `{ts, type, payload}`，且 flatten + 标签枚举可往返。
    #[test]
    fn record_roundtrips_with_flattened_tag() {
        let rec = Record::new(RecordKind::Message(Message::User("你好".into())));
        let line = serde_json::to_string(&rec).unwrap();
        assert!(line.contains("\"type\":\"message\""));
        assert!(line.contains("\"payload\""));
        assert!(line.contains("\"ts\""));
        let back: Record = serde_json::from_str(&line).unwrap();
        match back.kind {
            RecordKind::Message(Message::User(s)) => assert_eq!(s, "你好"),
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn meta_and_compacted_roundtrip() {
        let meta = SessionMeta {
            id: "abc".into(),
            parent_id: None,
            forked_from: Some("p".into()),
            cwd: "/x".into(),
            git: Some(GitInfo {
                commit: "deadbee".into(),
                branch: "main".into(),
            }),
            title: Some("标题".into()),
            carter_version: "0.1.0".into(),
            model: "ws/sonnet".into(),
            created_at: 123,
        };
        let line = serde_json::to_string(&Record::new(RecordKind::SessionMeta(meta))).unwrap();
        let back: Record = serde_json::from_str(&line).unwrap();
        assert!(matches!(back.kind, RecordKind::SessionMeta(_)));

        let comp = Record::new(RecordKind::Compacted {
            tier: "L3".into(),
            messages: vec![Message::Assistant("摘要".into())],
        });
        let back: Record = serde_json::from_str(&serde_json::to_string(&comp).unwrap()).unwrap();
        match back.kind {
            RecordKind::Compacted { tier, messages } => {
                assert_eq!(tier, "L3");
                assert_eq!(messages.len(), 1);
            }
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn cap_truncates_only_oversized_tool_output() {
        let small = Message::Tool {
            call_id: "1".into(),
            content: "ok".into(),
        };
        assert!(matches!(cap_for_persist(&small), Message::Tool { content, .. } if content == "ok"));

        let big = Message::Tool {
            call_id: "1".into(),
            content: "x".repeat(TOOL_OUTPUT_PERSIST_CAP + 10),
        };
        if let Message::Tool { content, .. } = cap_for_persist(&big) {
            assert!(content.contains("truncated for session log"));
            assert!(content.len() < TOOL_OUTPUT_PERSIST_CAP + 200);
        } else {
            panic!("expected tool msg");
        }

        // 非工具消息原样。
        let user = Message::User("hi".into());
        assert!(matches!(cap_for_persist(&user), Message::User(s) if s == "hi"));
    }
}
