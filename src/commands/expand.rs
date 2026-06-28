//! 命令展开。处理顺序（安全优先，仿 Gemini）：
//! 1. `@file` —— 注入文件内容（路径相对 cwd 解析）。
//! 2. `` !`cmd` `` —— 执行 shell 命令，注入其 stdout（命令文件由用户自撰，隐式信任）。
//! 3. `$ARGUMENTS` / `$1..$9` —— 替换调用参数；无占位符且有参数则把原始参数追加到末尾。

use std::path::Path;

use super::SlashCommand;

/// 展开命令正文。`raw_args` 为命令名之后的原始字符串；`cwd` 用于解析 `@相对路径`。
pub fn expand(cmd: &SlashCommand, raw_args: &str, cwd: &Path) -> String {
    let body = inject_files(&cmd.body, cwd);
    let body = inject_shell(&body, cwd);
    substitute_args(&body, raw_args)
}

/// 注入 `@path` 文件内容。仅当 `@` 在行首或空白后、且文件可读时替换；否则原样保留。
fn inject_files(body: &str, cwd: &Path) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let at_boundary = i == 0 || chars[i - 1].is_whitespace();
        if c == '@' && at_boundary {
            // 捕获 @ 后的非空白路径 token。
            let start = i + 1;
            let mut j = start;
            while j < chars.len() && !chars[j].is_whitespace() {
                j += 1;
            }
            let token: String = chars[start..j].iter().collect();
            if !token.is_empty() {
                let path = cwd.join(&token);
                if let Ok(content) = std::fs::read_to_string(&path) {
                    out.push_str(&format!("\n--- {token} ---\n{content}\n--- end {token} ---\n"));
                    i = j;
                    continue;
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 执行 `` !`cmd` `` 并把 stdout 注入原位。命令失败则注入错误标记（错误即数据）。
fn inject_shell(body: &str, cwd: &Path) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("!`") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('`') else {
            // 无闭合反引号：原样输出剩余。
            out.push_str(&rest[start..]);
            return out;
        };
        let cmd = &after[..end];
        out.push_str(&run_shell(cmd, cwd));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// 跑一条 shell 命令，返回 trim 后的 stdout（失败返回 `[cmd failed: ...]`）。
fn run_shell(cmd: &str, cwd: &Path) -> String {
    #[cfg(windows)]
    let mut command = {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(cmd);
        c
    };
    match command.current_dir(cwd).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim_end().to_string(),
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            format!("[cmd failed: {}]", err.trim())
        }
        Err(e) => format!("[cmd error: {e}]"),
    }
}

/// 替换 `$ARGUMENTS` / `$1..$9`；无占位符且有参数则追加原始参数。
fn substitute_args(body: &str, raw_args: &str) -> String {
    let raw_args = raw_args.trim();
    let tokens: Vec<&str> = raw_args.split_whitespace().collect();

    let has_placeholder = body.contains("$ARGUMENTS") || has_positional(body);

    let mut out = body.replace("$ARGUMENTS", raw_args);
    for i in 1..=9u8 {
        let pat = format!("${i}");
        if out.contains(&pat) {
            let val = tokens.get((i - 1) as usize).copied().unwrap_or("");
            out = out.replace(&pat, val);
        }
    }

    if !has_placeholder && !raw_args.is_empty() {
        out.push_str("\n\n");
        out.push_str(raw_args);
    }
    out
}

/// 正文是否含 `$1`..`$9` 位置占位符。
fn has_positional(body: &str) -> bool {
    (1..=9u8).any(|i| body.contains(&format!("${i}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Scope;

    fn cmd(body: &str) -> SlashCommand {
        SlashCommand {
            name: "t".into(),
            description: None,
            argument_hint: None,
            allowed_tools: Vec::new(),
            model: None,
            body: body.into(),
            scope: Scope::User,
        }
    }

    #[test]
    fn substitutes_all_arguments() {
        let c = cmd("修复：$ARGUMENTS");
        assert_eq!(expand(&c, "  登录 bug ", Path::new(".")), "修复：登录 bug");
    }

    #[test]
    fn substitutes_positional() {
        let c = cmd("scope=$1 type=$2");
        assert_eq!(
            expand(&c, "auth feat extra", Path::new(".")),
            "scope=auth type=feat"
        );
    }

    #[test]
    fn missing_positional_becomes_empty() {
        let c = cmd("a=$1 b=$2");
        assert_eq!(expand(&c, "only", Path::new(".")), "a=only b=");
    }

    #[test]
    fn appends_raw_when_no_placeholder() {
        let c = cmd("审查这段代码");
        assert_eq!(
            expand(&c, "src/main.rs", Path::new(".")),
            "审查这段代码\n\nsrc/main.rs"
        );
    }

    #[test]
    fn no_placeholder_no_args_unchanged() {
        let c = cmd("跑测试");
        assert_eq!(expand(&c, "", Path::new(".")), "跑测试");
    }

    #[test]
    fn injects_file_contents() {
        let dir = std::env::temp_dir().join(format!("carter-at-{}", crate::session::now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "hello-content").unwrap();
        let out = inject_files("看 @note.txt 这个文件", &dir);
        assert!(out.contains("hello-content"));
        assert!(out.contains("--- note.txt ---"));
        // 不存在的文件原样保留。
        let out2 = inject_files("@missing.txt", &dir);
        assert_eq!(out2, "@missing.txt");
        // 非边界的 @ 不触发（如邮箱）。
        let out3 = inject_files("a@b.txt", &dir);
        assert_eq!(out3, "a@b.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn injects_shell_output() {
        // echo 在 cmd(Windows) 与 sh(unix) 均可用。
        let out = inject_shell("结果：!`echo hi`！", Path::new("."));
        assert!(out.contains("hi"));
        assert!(!out.contains("echo"));
        // 无闭合反引号原样保留。
        let out2 = inject_shell("!`unclosed", Path::new("."));
        assert_eq!(out2, "!`unclosed");
    }
}
