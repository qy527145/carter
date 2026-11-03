//! 多层记忆 —— 受 Claude Code 的 CLAUDE.md / Codex 的 AGENTS.md 启发。
//! 发现并拼装分层记忆文件：全局（`~/.carter/CARTER.md` + facts.md + profile.md）
//! + 项目祖先链（从 cwd 逐级上溯到 git 根或文件系统根，每层的 `CARTER.md` / `AGENTS.md`）。
//! 作为单个 system 段注入，是持久背景与偏好。
//!
//! 两类内容：
//! - **Facts**：项目/环境事实（测试命令、包管理器、运行约定）
//! - **Profile**：用户画像（角色、偏好、沟通风格）
//!
//! 可复用的多步工作流不在这里 —— 那是技能（`skills/<name>/SKILL.md`）的事。
//!
//! 写路径要求：**原子写**（tmp + rename）+ **修订快照**（每次写前备份到
//! `~/.carter/memory_revisions/`），崩溃恢复最多读到上一版完整内容。
//!
//! 纪律：本模块不得 import `genai`/`rmcp`/`ratatui`/`crossterm`，仅 std。

use std::path::{Path, PathBuf};

pub mod writer;

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

/// 发现并渲染多层记忆为一个 system 段。
///
/// **永远非空**：哪怕一条记忆都没有，也要告诉模型「记忆机制在、往哪写」。空着不注入
/// 的话，模型连有这回事都不知道，save_memory 就几乎不会被调用。
pub fn load(cwd: &Path) -> String {
    let layers = discover(cwd);
    render(&layers, cwd)
}

/// 记忆的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 项目/环境事实：构建测试命令、约定、踩过的坑。
    Facts,
    /// 用户画像：身份、偏好、沟通风格。只有用户级。
    Profile,
}

/// 记忆的作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 当前项目：`<项目目录>/CARTER.md`。
    Project,
    /// 跨项目：`~/.carter/` 下。
    User,
}

impl Kind {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "facts" | "fact" => Ok(Self::Facts),
            "profile" => Ok(Self::Profile),
            other => Err(format!("未知 kind: {other}（仅支持 facts | profile）")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Facts => "facts",
            Self::Profile => "profile",
        }
    }
}

impl Scope {
    /// `global` 是旧名，等同 `user`。
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "project" => Ok(Self::Project),
            "user" | "global" => Ok(Self::User),
            other => Err(format!("未知 scope: {other}（仅支持 project | user）")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

/// 项目级记忆文件：`<project_dir>/CARTER.md`。
pub fn project_memory_path(project_dir: &Path) -> PathBuf {
    project_dir.join("CARTER.md")
}

/// (kind, scope) → 目标文件。profile 只有用户级，scope 被忽略。
pub fn target_path(kind: Kind, scope: Scope, project_dir: &Path) -> PathBuf {
    match (kind, scope) {
        (Kind::Profile, _) => crate::config::paths::global_profile_path(),
        (Kind::Facts, Scope::User) => crate::config::paths::global_facts_path(),
        (Kind::Facts, Scope::Project) => project_memory_path(project_dir),
    }
}

/// 一次写入的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    /// 已追加到该文件。
    Written(PathBuf),
    /// 文件里已有完全相同的一条，未重复写入。
    Duplicate(PathBuf),
}

/// 把一条记忆以 bullet 追加进目标文件（原子写 + 修订快照）。
///
/// 多行内容压成一行：记忆文件是给模型每轮读的，一条一行最省也最好去重。
pub fn save(content: &str, kind: Kind, scope: Scope, project_dir: &Path) -> Result<Saved, String> {
    let text = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return Err("记忆内容不能为空".into());
    }
    let path = target_path(kind, scope, project_dir);
    let bullet = format!("- {text}");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == bullet) {
        return Ok(Saved::Duplicate(path));
    }
    let updated = append_bullet(&existing, &bullet, kind);
    writer::write_atomic(&path, &updated)
        .map_err(|e| format!("写入 {} 失败：{e}", path.display()))?;
    Ok(Saved::Written(path))
}

/// 追加时归入的小节标题。
fn section_for(kind: Kind) -> &'static str {
    match kind {
        Kind::Facts => "## Facts",
        Kind::Profile => "## Profile",
    }
}

/// 把 bullet 追加进记忆正文：
/// - 空文件 → 建标题骨架（# 标题 + section）
/// - 无对应 section → 文末追加 section 头 + bullet
/// - 有 section → 直接追加在文末
fn append_bullet(existing: &str, bullet: &str, kind: Kind) -> String {
    let section = section_for(kind);
    if existing.trim().is_empty() {
        return format!(
            "# Carter 记忆 ({})\n\n{section}\n\n{bullet}\n",
            kind.as_str()
        );
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

/// 收集各层记忆：全局在最前（最外层），随后项目祖先链按外 → 内（cwd 最后、最就近）。
fn discover(cwd: &Path) -> Vec<Layer> {
    let mut layers = Vec::new();

    // 全局综合层：~/.carter/CARTER.md。
    if let Some(content) = read_capped(&crate::config::paths::global_memory_path()) {
        layers.push(Layer {
            label: "global".into(),
            content,
        });
    }
    // 全局事实层：~/.carter/facts.md。
    if let Some(content) = read_capped(&crate::config::paths::global_facts_path()) {
        layers.push(Layer {
            label: "global-facts".into(),
            content,
        });
    }
    // 全局画像层：~/.carter/profile.md（用户身份/偏好）。
    if let Some(content) = read_capped(&crate::config::paths::global_profile_path()) {
        layers.push(Layer {
            label: "global-profile".into(),
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

/// 渲染为带分层标注的 markdown 段：先说明往哪写，再列出已有各层。
fn render(layers: &[Layer], cwd: &Path) -> String {
    let mut out = String::from(
        "# 记忆（分层）\n\
         以下是全局与项目的持久记忆，视为背景与偏好；就近层级优先，但永不覆盖安全红线。\n\n\
         用 save_memory 写入，按内容选位置：\n",
    );
    out.push_str(&format!(
        "- 项目事实（scope=project）→ `{}`：本项目的命令、约定、坑\n",
        project_memory_path(cwd).display()
    ));
    out.push_str(&format!(
        "- 跨项目事实（scope=user）→ `{}`：本机环境、通用工具链\n",
        crate::config::paths::global_facts_path().display()
    ));
    out.push_str(&format!(
        "- 用户画像（kind=profile）→ `{}`：身份、偏好、沟通风格\n",
        crate::config::paths::global_profile_path().display()
    ));
    if layers.is_empty() {
        out.push_str("\n目前还没有任何记忆。");
        return out;
    }
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

        let rendered = render(&layers, &sub);
        assert!(rendered.contains("外层记忆") && rendered.contains("内层记忆"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn render_empty_still_tells_where_to_write() {
        let out = render(&[], Path::new("/tmp/proj"));
        assert!(out.contains("save_memory"));
        assert!(out.contains("CARTER.md"));
        assert!(out.contains("还没有任何记忆"));
    }

    #[test]
    fn append_to_empty_creates_skeleton() {
        let out = append_bullet("", "- 用户偏好中文回答", Kind::Facts);
        assert!(out.contains("# Carter 记忆"));
        assert!(out.contains("## Facts"));
        assert!(out.contains("- 用户偏好中文回答"));
    }

    #[test]
    fn append_adds_section_when_missing() {
        let out = append_bullet("# 既有记忆\n\n- 旧事实\n", "- 新事实", Kind::Facts);
        assert!(out.contains("- 旧事实"));
        assert!(out.contains("## Facts"));
        assert!(out.trim_end().ends_with("- 新事实"));
    }

    #[test]
    fn append_reuses_existing_section() {
        let out = append_bullet("# t\n\n## Facts\n\n- a\n", "- b", Kind::Facts);
        assert_eq!(out.matches("## Facts").count(), 1);
        assert!(out.contains("- a") && out.contains("- b"));
    }

    #[test]
    fn target_path_resolves_project_against_given_dir() {
        let dir = Path::new("/tmp/proj");
        assert_eq!(
            target_path(Kind::Facts, Scope::Project, dir),
            PathBuf::from("/tmp/proj/CARTER.md")
        );
        assert!(target_path(Kind::Facts, Scope::User, dir).ends_with("facts.md"));
        assert!(target_path(Kind::Profile, Scope::Project, dir).ends_with("profile.md"));
    }

    #[test]
    fn scope_accepts_legacy_global() {
        assert_eq!(Scope::parse("global").unwrap(), Scope::User);
        assert!(Scope::parse("nope").is_err());
        assert!(Kind::parse("skill").is_err());
    }

    #[test]
    fn save_writes_then_dedups() {
        let dir =
            std::env::temp_dir().join(format!("carter-mem-save-{}", crate::session::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = save("测试用  cargo test\n跑", Kind::Facts, Scope::Project, &dir).unwrap();
        assert_eq!(first, Saved::Written(dir.join("CARTER.md")));
        let body = std::fs::read_to_string(dir.join("CARTER.md")).unwrap();
        assert!(body.contains("- 测试用 cargo test 跑"));
        let again = save("测试用 cargo test 跑", Kind::Facts, Scope::Project, &dir).unwrap();
        assert!(matches!(again, Saved::Duplicate(_)));
        assert!(save("  ", Kind::Facts, Scope::Project, &dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
