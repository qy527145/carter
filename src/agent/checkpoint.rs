//! Checkpoint —— 写前文件快照 + `/rewind` 撤销。
//! 在 `write_file`/`edit_file` 执行**前**抓取目标文件的当前内容，
//! `/rewind <n>` 可把文件恢复到某个检查点之前的状态。
//!
//! 范围：仅覆盖结构化文件工具（bash 内的 `rm`/重定向等无法获知路径，不在此列）。
//! 当前为**会话内内存**存储（不跨重启 / resume）；只回滚文件，不回滚对话历史。

use std::path::{Path, PathBuf};

/// 单个文件在某次改动前的内容快照。
#[derive(Debug, Clone)]
pub struct FileSnapshot {
    pub path: PathBuf,
    /// 改动前内容；None = 当时文件不存在（回滚即删除）。
    pub prior: Option<String>,
}

/// 一次工具调用产生的检查点（可能涉及多个文件）。
#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub label: String,
    pub snapshots: Vec<FileSnapshot>,
}

/// 会话级检查点栈。
#[derive(Debug, Default)]
pub struct CheckpointStore {
    entries: Vec<Checkpoint>,
}

impl CheckpointStore {
    /// 工具执行前调用：抓取将被改动文件的当前内容，压入一个检查点。
    /// 读不到的路径记为 `prior = None`（视作不存在）。`paths` 为空则不记。
    pub fn snapshot(&mut self, label: impl Into<String>, paths: &[PathBuf]) {
        if paths.is_empty() {
            return;
        }
        let snapshots = paths
            .iter()
            .map(|p| FileSnapshot {
                path: p.clone(),
                prior: std::fs::read_to_string(p).ok(),
            })
            .collect();
        self.entries.push(Checkpoint {
            label: label.into(),
            snapshots,
        });
    }

    pub fn list(&self) -> &[Checkpoint] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 回滚到检查点 `index`（1 基）**之前**的状态：撤销 index 及其后的所有文件改动。
    /// 倒序应用各快照的 prior（恢复内容 / 删除新建文件），随后截断已撤销的检查点。
    /// 返回恢复的文件数；index 越界返回 Err。
    pub fn rewind_to(&mut self, index: usize) -> Result<usize, String> {
        if index < 1 || index > self.entries.len() {
            return Err(format!(
                "检查点序号 {index} 越界（当前 1..={}）",
                self.entries.len()
            ));
        }
        let mut restored = 0usize;
        // 倒序：靠后的先恢复，最早的 prior 最后落地 → 文件回到 index 之前。
        for cp in self.entries[index - 1..].iter().rev() {
            for snap in &cp.snapshots {
                if restore(&snap.path, snap.prior.as_deref()).is_ok() {
                    restored += 1;
                }
            }
        }
        self.entries.truncate(index - 1);
        Ok(restored)
    }
}

/// 把单个文件恢复到 `prior`：Some → 写回内容；None → 删除（若存在）。
fn restore(path: &Path, prior: Option<&str>) -> std::io::Result<()> {
    match prior {
        Some(content) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, content)
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            // 本就不存在 → 视作成功。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        },
    }
}

/// 从工具名 + 参数推断将被改动的文件路径（仅结构化文件工具）。
pub fn mutating_paths(name: &str, args: &serde_json::Value) -> Vec<PathBuf> {
    match name {
        "write_file" | "edit_file" => args
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default(),
        // save_memory 追加写 CARTER.md：按 scope 推断路径，纳入 /rewind。
        "save_memory" => {
            let path = match args.get("scope").and_then(|v| v.as_str()).unwrap_or("project") {
                "global" => crate::config::paths::global_memory_path(),
                _ => PathBuf::from("CARTER.md"),
            };
            vec![path]
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "carter-ckpt-{tag}-{}",
            crate::session::now_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn mutating_paths_extracts_path() {
        assert_eq!(
            mutating_paths("write_file", &json!({"path": "a.rs", "content": "x"})),
            vec![PathBuf::from("a.rs")]
        );
        assert_eq!(
            mutating_paths("edit_file", &json!({"path": "b.rs"})),
            vec![PathBuf::from("b.rs")]
        );
        assert!(mutating_paths("bash", &json!({"command": "rm x"})).is_empty());
    }

    #[test]
    fn rewind_restores_modified_and_deletes_created() {
        let dir = tmp_dir("rewind");
        let existing = dir.join("keep.txt");
        let created = dir.join("new.txt");
        std::fs::write(&existing, "原始内容").unwrap();

        let mut store = CheckpointStore::default();
        // cp1：改 existing 前快照（prior = "原始内容"）。
        store.snapshot("edit keep", &[existing.clone()]);
        std::fs::write(&existing, "被改坏了").unwrap();
        // cp2：建 created 前快照（prior = None）。
        store.snapshot("write new", &[created.clone()]);
        std::fs::write(&created, "新文件").unwrap();

        assert_eq!(store.len(), 2);
        // 回滚到 cp1 之前。
        let n = store.rewind_to(1).unwrap();
        assert_eq!(n, 2);
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "原始内容");
        assert!(!created.exists()); // 新建文件被删
        assert!(store.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewind_partial_keeps_earlier_checkpoints() {
        let dir = tmp_dir("partial");
        let f = dir.join("f.txt");
        std::fs::write(&f, "v0").unwrap();
        let mut store = CheckpointStore::default();
        store.snapshot("e1", &[f.clone()]); // prior v0
        std::fs::write(&f, "v1").unwrap();
        store.snapshot("e2", &[f.clone()]); // prior v1
        std::fs::write(&f, "v2").unwrap();
        // 回滚到 cp2 之前 → 文件回到 v1，cp1 保留。
        store.rewind_to(2).unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1");
        assert_eq!(store.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewind_out_of_range_errs() {
        let mut store = CheckpointStore::default();
        assert!(store.rewind_to(1).is_err());
    }

    #[test]
    fn empty_paths_no_checkpoint() {
        let mut store = CheckpointStore::default();
        store.snapshot("noop", &[]);
        assert!(store.is_empty());
    }
}
