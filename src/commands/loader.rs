//! 自定义命令发现与解析。项目级（`.carter/commands/`）覆盖用户级（`~/.carter/commands/`）。
//! 文件名（去 `.md`）即命令名；子目录形成 `ns:name` 命名空间。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::paths::carter_home;

use super::{Scope, SlashCommand};

/// 用户级命令目录 `~/.carter/commands`。
fn user_commands_dir() -> PathBuf {
    carter_home().join("commands")
}

/// 项目级命令目录 `<cwd>/.carter/commands`。
fn project_commands_dir(cwd: &Path) -> PathBuf {
    cwd.join(".carter").join("commands")
}

/// 发现全部自定义命令。先加载用户级、再项目级覆盖（同名项目胜）。按名排序返回。
pub fn discover(cwd: &Path) -> Vec<SlashCommand> {
    let mut map: BTreeMap<String, SlashCommand> = BTreeMap::new();
    for cmd in load_dir(&user_commands_dir(), Scope::User) {
        map.insert(cmd.name.clone(), cmd);
    }
    for cmd in load_dir(&project_commands_dir(cwd), Scope::Project) {
        map.insert(cmd.name.clone(), cmd); // 项目覆盖用户
    }
    map.into_values().collect()
}

/// 扫描一个目录下所有 `**/*.md`，解析为命令。
fn load_dir(dir: &Path, scope: Scope) -> Vec<SlashCommand> {
    let mut out = Vec::new();
    let pat = dir.join("**").join("*.md");
    let Ok(paths) = glob::glob(&pat.to_string_lossy()) else {
        return out;
    };
    for path in paths.flatten() {
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(name) = command_name(dir, &path) else {
            continue;
        };
        out.push(parse(name, &raw, scope));
    }
    out
}

/// 相对 `dir` 的路径 → 命令名：去扩展名，路径分隔符换 `:`。
fn command_name(dir: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(dir).ok()?;
    let stem_parent: Vec<String> = rel
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    if stem_parent.is_empty() {
        return None;
    }
    Some(stem_parent.join(":"))
}

/// 解析单个命令文件：可选 frontmatter（`---` 包裹）+ 正文。
fn parse(name: String, raw: &str, scope: Scope) -> SlashCommand {
    let (front, body) = split_frontmatter(raw);
    let mut cmd = SlashCommand {
        name,
        description: None,
        argument_hint: None,
        allowed_tools: Vec::new(),
        model: None,
        body: body.trim().to_string(),
        scope,
    };
    for (key, val) in front {
        match key.as_str() {
            "description" => cmd.description = Some(val),
            "argument-hint" | "argument_hint" => cmd.argument_hint = Some(val),
            "allowed-tools" | "allowed_tools" => cmd.allowed_tools = parse_list(&val),
            "model" => cmd.model = Some(val),
            _ => {}
        }
    }
    cmd
}

/// 切分 frontmatter。首行须为 `---`，到下一行 `---` 为止。返回 (键值对, 正文)。
fn split_frontmatter(raw: &str) -> (Vec<(String, String)>, &str) {
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(raw); // 去 BOM
    let rest = match trimmed.strip_prefix("---\n").or_else(|| trimmed.strip_prefix("---\r\n")) {
        Some(r) => r,
        None => return (Vec::new(), trimmed),
    };
    // 找闭合 `---`。
    let mut pairs = Vec::new();
    let mut body_start = None;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        if content.trim() == "---" {
            body_start = Some(offset + line.len());
            break;
        }
        if let Some((k, v)) = content.split_once(':') {
            let v = v.trim().trim_matches(['"', '\'']).to_string();
            pairs.push((k.trim().to_string(), v));
        }
        offset += line.len();
    }
    match body_start {
        Some(s) => (pairs, &rest[s..]),
        // 无闭合 ---：当作无 frontmatter。
        None => (Vec::new(), trimmed),
    }
}

/// 解析 YAML 内联列表 `[a, b]` 或裸单值/逗号分隔。
fn parse_list(val: &str) -> Vec<String> {
    let inner = val.trim();
    let inner = inner
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(inner);
    inner
        .split(',')
        .map(|s| s.trim().trim_matches(['"', '\'']).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let raw = "---\ndescription: 提交\nargument-hint: [scope]\nallowed-tools: [bash, read_file]\nmodel: fast\n---\n基于 $1 提交。\n";
        let cmd = parse("git:commit".into(), raw, Scope::Project);
        assert_eq!(cmd.name, "git:commit");
        assert_eq!(cmd.description.as_deref(), Some("提交"));
        assert_eq!(cmd.argument_hint.as_deref(), Some("[scope]"));
        assert_eq!(cmd.allowed_tools, vec!["bash", "read_file"]);
        assert_eq!(cmd.model.as_deref(), Some("fast"));
        assert_eq!(cmd.body, "基于 $1 提交。");
    }

    #[test]
    fn no_frontmatter_is_all_body() {
        let cmd = parse("foo".into(), "just a prompt\n", Scope::User);
        assert!(cmd.description.is_none());
        assert_eq!(cmd.body, "just a prompt");
    }

    #[test]
    fn unterminated_frontmatter_is_body() {
        let cmd = parse("foo".into(), "---\ndescription: x\nbody here", Scope::User);
        assert!(cmd.description.is_none());
        assert!(cmd.body.contains("description: x"));
    }

    #[test]
    fn command_name_namespaces_subdirs() {
        let dir = Path::new("/c");
        assert_eq!(
            command_name(dir, Path::new("/c/git/commit.md")).as_deref(),
            Some("git:commit")
        );
        assert_eq!(command_name(dir, Path::new("/c/foo.md")).as_deref(), Some("foo"));
    }

    #[test]
    fn parse_list_handles_forms() {
        assert_eq!(parse_list("[a, b]"), vec!["a", "b"]);
        assert_eq!(parse_list("bash"), vec!["bash"]);
        assert_eq!(parse_list("[]"), Vec::<String>::new());
    }
}
