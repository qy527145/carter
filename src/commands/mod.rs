//! 斜杠命令 —— 内置命令注册表 + 自定义命令（Markdown + frontmatter）加载与展开。
//! 设计见 docs/05-slash-commands.md。本模块是 agent 的下层，纯逻辑、不碰终端。

mod expand;
mod loader;

pub use expand::expand;
pub use loader::discover;

/// 命令作用域。项目级覆盖用户级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `.carter/commands/`（cwd 内，可随仓库提交）。
    Project,
    /// `~/.carter/commands/`（用户全局）。
    User,
}

/// 一条自定义斜杠命令。
#[derive(Debug, Clone)]
pub struct SlashCommand {
    /// 调用名（含命名空间），如 `git:commit`。
    pub name: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
    /// prompt 模板正文。
    pub body: String,
    /// 来源作用域（保留供菜单区分项目/用户）。
    #[allow(dead_code)]
    pub scope: Scope,
}

/// 内置命令元数据（供补全菜单 + TUI 分发共用）。dispatch 行为在 TUI 层。
#[derive(Debug, Clone, Copy)]
pub struct Builtin {
    pub name: &'static str,
    pub description: &'static str,
}

/// 全部内置命令（顺序即菜单展示顺序，常用在前）。
pub const BUILTINS: &[Builtin] = &[
    Builtin { name: "help", description: "显示可用命令" },
    Builtin { name: "clear", description: "清空当前上下文（磁盘原文留存）" },
    Builtin { name: "compact", description: "压缩对话历史" },
    Builtin { name: "context", description: "查看上下文占用" },
    Builtin { name: "cost", description: "查看本会话用量与成本" },
    Builtin { name: "model", description: "切换模型" },
    Builtin { name: "new", description: "开启新会话" },
    Builtin { name: "resume", description: "恢复历史会话" },
    Builtin { name: "fork", description: "从当前会话派生新会话" },
    Builtin { name: "rewind", description: "撤销文件改动（回滚到某检查点之前）" },
    Builtin { name: "skills", description: "列出可用 skills" },
    Builtin { name: "mcp", description: "查看 MCP server 状态" },
    Builtin { name: "quit", description: "退出" },
    Builtin { name: "exit", description: "退出" },
];

/// 是否为内置命令名。
pub fn is_builtin(name: &str) -> bool {
    BUILTINS.iter().any(|b| b.name == name)
}
