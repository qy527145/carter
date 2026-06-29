//! save_memory：把一条事实 / 偏好 / 工作流追加进分层记忆文件，跨会话持久。
//!
//! 三态分层（对齐 aiko-agent）：
//! - `kind=facts` (默认)：项目/环境事实 → 项目级 `<cwd>/CARTER.md` 的 Facts 段
//!                          或全局 `~/.carter/facts.md`
//! - `kind=profile`：用户画像 → 仅全局 `~/.carter/profile.md`
//! - `kind=skill`：可复用工作流 → `~/.carter/skills/<slug>.md`
//!
//! 写路径用原子写 + 修订快照（见 `memory::writer::write_atomic`）。

use serde_json::{json, Value};

use super::{Tool, ToolResult};

/// save_memory 追加的归集小节标题（兼容老 CARTER.md 文件，新写入仍按 kind 分段）。
const FACTS_SECTION: &str = "## Facts";
const PROFILE_SECTION: &str = "## Profile";

pub struct SaveMemory;

/// 解析 (kind, scope) → 目标记忆文件路径 + 段标题。
/// 返回 (path, section)；skill kind 不用 section（整文件就是一个 skill）。
fn target_for(kind: &str, scope: &str, slug: Option<&str>) -> Result<(std::path::PathBuf, Option<&'static str>), String> {
    match kind {
        "facts" => match scope {
            "project" => Ok((std::path::PathBuf::from("CARTER.md"), Some(FACTS_SECTION))),
            "global" => Ok((crate::config::paths::global_facts_path(), None)),
            other => Err(format!("未知 scope: {other}（仅支持 project | global）")),
        },
        "profile" => Ok((crate::config::paths::global_profile_path(), None)),
        "skill" => {
            let slug = slug.unwrap_or("").trim();
            if slug.is_empty() {
                return Err("kind=skill 必须提供 slug 参数（如 \"rust-tests\"）".to_string());
            }
            // 仅允许 url-safe 字符避免逃逸目录。
            if !slug.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return Err("slug 只能包含 [A-Za-z0-9_-]".to_string());
            }
            Ok((crate::config::paths::skill_memory_path(slug), None))
        }
        other => Err(format!("未知 kind: {other}（仅支持 facts | profile | skill）")),
    }
}

#[async_trait::async_trait]
impl Tool for SaveMemory {
    fn name(&self) -> &str {
        "save_memory"
    }

    fn description(&self) -> &str {
        "把跨会话持久信息写入记忆文件。三种 kind：\n\
         - facts (默认)：项目/环境事实（测试命令、约定）；scope=project 写 CARTER.md，global 写 ~/.carter/facts.md\n\
         - profile：用户画像、偏好；仅 global\n\
         - skill：可复用工作流；写 ~/.carter/skills/<slug>.md（slug 必填）\n\n\
         写入原子（tmp+rename），自动备份上一版到 ~/.carter/memory_revisions/。\
         只在确属持久、可复用的信息时使用；一次只记一条精炼的内容。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "要记住的内容（facts/profile 一句话；skill 多段说明）" },
                "fact": { "type": "string", "description": "（兼容旧版别名）等同于 content" },
                "kind": {
                    "type": "string",
                    "enum": ["facts", "profile", "skill"],
                    "description": "facts (默认) / profile / skill"
                },
                "scope": {
                    "type": "string",
                    "enum": ["project", "global"],
                    "description": "仅 kind=facts 时生效：project=当前项目 CARTER.md（默认）；global=~/.carter/facts.md"
                },
                "slug": {
                    "type": "string",
                    "description": "仅 kind=skill 时生效：唯一 id，文件名 ~/.carter/skills/<slug>.md（只能 [A-Za-z0-9_-]）"
                }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        // content 是新主参数；fact 是旧别名兼容。
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .or_else(|| args.get("fact").and_then(Value::as_str))
            .map(str::trim)
            .unwrap_or("");
        if content.is_empty() {
            return ToolResult::err("content 不能为空（旧名 fact 也可）");
        }
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("facts");
        let scope = args.get("scope").and_then(Value::as_str).unwrap_or("project");
        let slug = args.get("slug").and_then(Value::as_str);

        let (path, section) = match target_for(kind, scope, slug) {
            Ok(t) => t,
            Err(e) => return ToolResult::err(e),
        };

        let updated = match kind {
            "skill" => {
                // skill：整文件就是这一项（覆盖式；若已存在等于编辑该 skill）。
                build_skill_file(slug.unwrap_or(""), content)
            }
            _ => {
                // facts / profile：以 bullet append；单行事实压成一行。
                let bullet_text = content.split_whitespace().collect::<Vec<_>>().join(" ");
                let bullet = format!("- {bullet_text}");
                let existing = std::fs::read_to_string(&path).unwrap_or_default();
                // 去重：逐行精确比对（trim）。
                if existing.lines().any(|l| l.trim() == bullet) {
                    return ToolResult::ok(format!(
                        "已存在相同记忆（{kind}/{scope}），未重复写入：{bullet_text}"
                    ));
                }
                append_bullet(&existing, &bullet, section, kind)
            }
        };

        match crate::memory::writer::write_atomic(&path, &updated) {
            Ok(()) => ToolResult::ok(format!("已记住（{kind} → {}）", path.display())),
            Err(e) => ToolResult::err(format!("写入 {} 失败：{e}", path.display())),
        }
    }
}

/// 把 bullet 追加进记忆正文：
/// - 空文件 → 建标题骨架（# kind 标题 + section）
/// - 无对应 section → 追加 section 头 + bullet
/// - 有 section → 直接 append
fn append_bullet(existing: &str, bullet: &str, section: Option<&str>, kind: &str) -> String {
    let section = section.unwrap_or(if kind == "profile" {
        PROFILE_SECTION
    } else {
        FACTS_SECTION
    });
    if existing.trim().is_empty() {
        return format!("# Carter 记忆 ({kind})\n\n{section}\n\n{bullet}\n");
    }
    let mut out = existing.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.contains(section) {
        out.push_str(&format!("\n{section}\n"));
    }
    out.push_str(&format!("{bullet}\n"));
    out
}

/// skill 文件骨架：单文件 = 一个 skill，覆盖式写入。
fn build_skill_file(slug: &str, content: &str) -> String {
    format!("# Skill: {slug}\n\n{content}\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_to_empty_creates_skeleton() {
        let out = append_bullet("", "- 用户偏好中文回答", None, "facts");
        assert!(out.contains("# Carter 记忆"));
        assert!(out.contains(FACTS_SECTION));
        assert!(out.contains("- 用户偏好中文回答"));
    }

    #[test]
    fn append_adds_section_when_missing() {
        let out = append_bullet("# 既有记忆\n\n- 旧事实\n", "- 新事实", None, "facts");
        assert!(out.contains("- 旧事实"));
        assert!(out.contains(FACTS_SECTION));
        assert!(out.trim_end().ends_with("- 新事实"));
    }

    #[test]
    fn append_reuses_existing_section() {
        let base = format!("# t\n\n{FACTS_SECTION}\n\n- a\n");
        let out = append_bullet(&base, "- b", None, "facts");
        // 不应重复出现小节标题。
        assert_eq!(out.matches(FACTS_SECTION).count(), 1);
        assert!(out.contains("- a") && out.contains("- b"));
    }

    #[test]
    fn target_for_resolves_kinds() {
        // facts/project → CARTER.md + Facts section
        let (p, s) = target_for("facts", "project", None).unwrap();
        assert_eq!(p, std::path::PathBuf::from("CARTER.md"));
        assert_eq!(s, Some(FACTS_SECTION));

        // facts/global → facts.md
        let (p, s) = target_for("facts", "global", None).unwrap();
        assert!(p.ends_with("facts.md"));
        assert_eq!(s, None);

        // profile → profile.md (scope ignored)
        let (p, _) = target_for("profile", "project", None).unwrap();
        assert!(p.ends_with("profile.md"));

        // skill 必填 slug
        assert!(target_for("skill", "global", None).is_err());
        let (p, _) = target_for("skill", "global", Some("rust-tests")).unwrap();
        assert!(p.to_string_lossy().contains("skills"));
        assert!(p.to_string_lossy().ends_with("rust-tests.md"));

        // slug 必须 url-safe
        assert!(target_for("skill", "global", Some("../etc/passwd")).is_err());
        assert!(target_for("skill", "global", Some("with space")).is_err());

        // 未知 kind
        assert!(target_for("unknown", "project", None).is_err());
    }

    #[test]
    fn build_skill_file_has_header() {
        let out = build_skill_file("rust-tests", "Step 1: cargo test\nStep 2: ...");
        assert!(out.starts_with("# Skill: rust-tests"));
        assert!(out.contains("Step 1"));
    }
}
