//! Skills —— 可发现能力包。每个 skill 是 `<skills_dir>/<name>/SKILL.md`
//! （YAML frontmatter + markdown 正文）。目录注入 system prompt 做发现；
//! `skill` 工具按名加载正文做按需展开。
//! 纪律：本模块不得 import `genai`/`rmcp`/`ratatui`/`crossterm`，仅 std/serde_json。

mod skill_tool;

pub use skill_tool::SkillTool;

use std::path::Path;

/// skill 元数据（frontmatter）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    /// advisory：本轮仅解析不强制。
    pub allowed_tools: Vec<String>,
}

/// 完整 skill：元数据 + markdown 正文。
#[derive(Debug, Clone)]
pub struct Skill {
    /// 当前仅 `body` 被消费；`meta` 预留给后续 allowed-tools 强制。
    #[allow(dead_code)]
    pub meta: SkillMeta,
    pub body: String,
}

/// 扫描 `<dir>/*/SKILL.md`，只解析 frontmatter。目录不存在 → 空。
pub fn discover(dir: &Path) -> Vec<SkillMeta> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let md = entry.path().join("SKILL.md");
        if let Ok(raw) = std::fs::read_to_string(&md) {
            if let Some((meta, _body)) = parse_skill_md(&raw) {
                out.push(meta);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 按名加载完整 skill（`<dir>/<name>/SKILL.md`）。
pub fn load(dir: &Path, name: &str) -> Option<Skill> {
    let md = dir.join(name).join("SKILL.md");
    let raw = std::fs::read_to_string(md).ok()?;
    let (meta, body) = parse_skill_md(&raw)?;
    Some(Skill { meta, body })
}

/// 手写解析 frontmatter：剥离首部 `---\n…\n---\n`，块内每行按首个 `:` 切分。
/// `allowed-tools: a, b, c` 单行逗号形式。缺 frontmatter 或缺 name → None。
pub fn parse_skill_md(raw: &str) -> Option<(SkillMeta, String)> {
    let rest = raw.strip_prefix("---\n").or_else(|| raw.strip_prefix("---\r\n"))?;
    // 找结束分隔行 `---`。
    let mut front = String::new();
    let mut body = String::new();
    let mut in_front = true;
    for line in rest.lines() {
        if in_front && (line.trim_end() == "---") {
            in_front = false;
            continue;
        }
        if in_front {
            front.push_str(line);
            front.push('\n');
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    // 没遇到结束分隔 → 非法 frontmatter。
    if in_front {
        return None;
    }

    let mut name = None;
    let mut description = String::new();
    let mut allowed_tools = Vec::new();
    for line in front.lines() {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();
        match key {
            "name" => name = Some(val.to_string()),
            "description" => description = val.to_string(),
            "allowed-tools" => {
                allowed_tools = val
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            _ => {}
        }
    }

    let name = name.filter(|n| !n.is_empty())?;
    Some((
        SkillMeta {
            name,
            description,
            allowed_tools,
        },
        body.trim_end().to_string(),
    ))
}

/// 渲染 skill 目录为 system prompt 片段（name + description 行）。空列表 → 空串。
pub fn render_catalog(metas: &[SkillMeta]) -> String {
    if metas.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "可用 Skills（按需用 `skill` 工具传入对应 name 加载完整说明后再执行）：\n",
    );
    for m in metas {
        out.push_str(&format!("- {}: {}\n", m.name, m.description));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let raw = "---\nname: demo\ndescription: 演示 skill\nallowed-tools: read_file, bash\n---\n正文第一行\n正文第二行\n";
        let (meta, body) = parse_skill_md(raw).unwrap();
        assert_eq!(meta.name, "demo");
        assert_eq!(meta.description, "演示 skill");
        assert_eq!(meta.allowed_tools, vec!["read_file", "bash"]);
        assert_eq!(body, "正文第一行\n正文第二行");
    }

    #[test]
    fn none_without_frontmatter() {
        assert!(parse_skill_md("没有 frontmatter 的普通 markdown").is_none());
    }

    #[test]
    fn none_when_name_missing() {
        let raw = "---\ndescription: 无名\n---\nbody\n";
        assert!(parse_skill_md(raw).is_none());
    }

    #[test]
    fn none_when_frontmatter_unterminated() {
        let raw = "---\nname: x\ndescription: y\nbody no end marker\n";
        assert!(parse_skill_md(raw).is_none());
    }

    #[test]
    fn allowed_tools_optional() {
        let raw = "---\nname: x\ndescription: y\n---\nbody\n";
        let (meta, _) = parse_skill_md(raw).unwrap();
        assert!(meta.allowed_tools.is_empty());
    }

    #[test]
    fn render_catalog_lists_entries() {
        let metas = vec![
            SkillMeta {
                name: "a".into(),
                description: "first".into(),
                allowed_tools: vec![],
            },
            SkillMeta {
                name: "b".into(),
                description: "second".into(),
                allowed_tools: vec![],
            },
        ];
        let cat = render_catalog(&metas);
        assert!(cat.contains("- a: first"));
        assert!(cat.contains("- b: second"));
    }

    #[test]
    fn render_catalog_empty_is_empty() {
        assert_eq!(render_catalog(&[]), "");
    }
}
