//! 多层记忆 —— 受 Claude Code 的 CLAUDE.md / Codex 的 AGENTS.md 启发。
//! 发现并拼装分层记忆文件：全局（`~/.carter/CARTER.md`）+ 项目祖先链
//! （从 cwd 逐级上溯到 git 根或文件系统根，每层的 `CARTER.md` / `AGENTS.md`）。
//! 作为单个 system 段注入，是持久背景与偏好。
//! 纪律：本模块不得 import `genai`/`rmcp`/`ratatui`/`crossterm`，仅 std。

use std::path::{Path, PathBuf};

/// 每层记忆的文件名候选（同一目录两者都在则都收）。
const MEMORY_FILES: [&str; 2] = ["CARTER.md", "AGENTS.md"];

/// 上溯收集的目录层数上限（含 cwd），防极深路径下读盘失控。
const MAX_DEPTH: usize = 8;

/// 单个记忆文件读入的字节上限（超出截断，避免淹没上下文）。
const MAX_FILE_BYTES: usize = 32 * 1024;

/// 一条已加载的记忆层。
struct Layer {
    /// 人类可读来源标签（`global` / 绝对路径）。
    label: String,
    content: String,
}

/// 发现并渲染多层记忆为一个 system 段。无任何记忆 → 空串（调用方据此不注入该段）。
pub fn load(cwd: &Path) -> String {
    let layers = discover(cwd);
    render(&layers)
}

/// 收集各层记忆：全局在最前（最外层），随后项目祖先链按外 → 内（cwd 最后、最就近）。
fn discover(cwd: &Path) -> Vec<Layer> {
    let mut layers = Vec::new();

    // 全局层：~/.carter/CARTER.md。
    if let Some(content) = read_capped(&crate::config::paths::global_memory_path()) {
        layers.push(Layer {
            label: "global".into(),
            content,
        });
    }

    // 项目祖先链：从 cwd 上溯（遇 git 根则停在该层），收集后反转成外 → 内。
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut cur = Some(cwd.to_path_buf());
    while let Some(dir) = cur {
        dirs.push(dir.clone());
        if dirs.len() >= MAX_DEPTH || dir.join(".git").exists() {
            break;
        }
        cur = dir.parent().map(Path::to_path_buf);
    }
    dirs.reverse();

    for dir in dirs {
        for name in MEMORY_FILES {
            let path = dir.join(name);
            if let Some(content) = read_capped(&path) {
                layers.push(Layer {
                    label: path.display().to_string(),
                    content,
                });
            }
        }
    }

    layers
}

/// 渲染为带分层标注的 markdown 段。空 → 空串。
fn render(layers: &[Layer]) -> String {
    if layers.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "# 记忆（分层）\n以下是全局与项目的持久记忆，视为背景与偏好；就近层级优先，但永不覆盖安全红线。\n",
    );
    for l in layers {
        out.push_str(&format!("\n## [{}]\n{}\n", l.label, l.content));
    }
    out.trim_end().to_string()
}

/// 读文件，去首尾空白后非空才返回；超长按字节截断（在字符边界处）。
fn read_capped(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= MAX_FILE_BYTES {
        return Some(trimmed.to_string());
    }
    // 截断到 <= MAX_FILE_BYTES 的字符边界。
    let mut end = MAX_FILE_BYTES;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}\n…（已截断）", &trimmed[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_project_layer_and_orders_inner_last() {
        let root = std::env::temp_dir().join(format!("carter-mem-{}", crate::session::now_ms()));
        let sub = root.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("CARTER.md"), "外层记忆").unwrap();
        std::fs::write(sub.join("CARTER.md"), "内层记忆").unwrap();

        let layers = discover(&sub);
        // 至少包含两层项目记忆；内层应排在外层之后。
        let outer = layers.iter().position(|l| l.content == "外层记忆");
        let inner = layers.iter().position(|l| l.content == "内层记忆");
        assert!(outer.is_some() && inner.is_some());
        assert!(outer.unwrap() < inner.unwrap(), "外层应在内层之前");

        let rendered = render(&layers);
        assert!(rendered.contains("外层记忆") && rendered.contains("内层记忆"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_empty_is_empty() {
        assert_eq!(render(&[]), "");
    }
}
