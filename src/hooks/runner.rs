//! Hook 命令执行器：把 payload JSON 经 stdin 喂给 shell 子进程，按退出码 + stdout 解释决策。

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt;

use super::types::{HookConfig, HookDecision, HookEvent, HookOutcome, HookPayload};

/// 执行单个 hook，返回决策。
/// 失败 / 超时 / 非协议退出码 → 记 warn，按 `Continue` 处理（坏 hook 不杀会话）。
pub async fn run_hook(cfg: &HookConfig, payload: &HookPayload) -> HookOutcome {
    if !cfg.enabled {
        return HookOutcome {
            decision: HookDecision::Continue,
            stdout_preview: String::new(),
            stderr_preview: String::new(),
            exit_code: 0,
        };
    }
    if !matches_filter(cfg, payload) {
        return HookOutcome {
            decision: HookDecision::Continue,
            stdout_preview: String::new(),
            stderr_preview: String::new(),
            exit_code: 0,
        };
    }

    // 1. 序列化 payload。
    let stdin_buf = match serde_json::to_vec(payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("hooks: failed to serialize payload ({e}); skipping hook");
            return HookOutcome {
                decision: HookDecision::Continue,
                stdout_preview: String::new(),
                stderr_preview: String::new(),
                exit_code: 0,
            };
        }
    };

    // 2. spawn shell 子进程。
    let (program, flag) = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
    let mut child = match tokio::process::Command::new(program)
        .arg(flag)
        .arg(&cfg.command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("hooks: failed to spawn `{}`: {e}", cfg.command);
            return HookOutcome {
                decision: HookDecision::Continue,
                stdout_preview: String::new(),
                stderr_preview: String::new(),
                exit_code: -1,
            };
        }
    };

    // 3. 写 stdin 后立即关闭（防止 hook 阻塞等待 EOF）。
    if let Some(mut stdin) = child.stdin.take() {
        let buf = stdin_buf.clone();
        // 异步写：忽略 broken pipe（hook 不读 stdin 也是合法的）。
        let _ = stdin.write_all(&buf).await;
        drop(stdin);
    }

    // 4. 等结果，超时一律 kill + 当 Continue。
    let wait_fut = child.wait_with_output();
    let output = match tokio::time::timeout(Duration::from_secs(cfg.timeout_secs), wait_fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!("hooks: child wait error ({e})");
            return HookOutcome {
                decision: HookDecision::Continue,
                stdout_preview: String::new(),
                stderr_preview: String::new(),
                exit_code: -1,
            };
        }
        Err(_) => {
            tracing::warn!(
                "hooks: `{}` timed out after {}s; treating as Continue",
                cfg.command,
                cfg.timeout_secs
            );
            return HookOutcome {
                decision: HookDecision::Continue,
                stdout_preview: String::new(),
                stderr_preview: String::new(),
                exit_code: -1,
            };
        }
    };

    let exit = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !stderr.trim().is_empty() {
        tracing::warn!("hooks: `{}` stderr: {}", cfg.command, stderr.trim());
    }

    let decision = decide(payload.event, exit, &stdout);
    HookOutcome {
        decision,
        stdout_preview: truncate_for_log(&stdout, 500),
        stderr_preview: truncate_for_log(&stderr, 500),
        exit_code: exit,
    }
}

/// 解析 hook 退出码 + stdout → 决策。
fn decide(event: HookEvent, exit_code: i32, stdout: &str) -> HookDecision {
    match exit_code {
        0 => {
            // 退出 0：尝试解析 stdout 为 JSON。
            // 协议：`{ "decision": "rewrite", "data": { ... } }` 或裸 `{ ... }`（兼容简化）。
            // 仅 rewritable 事件生效；其它 stdout 内容忽略。
            if !event.is_rewritable() || stdout.trim().is_empty() {
                return HookDecision::Continue;
            }
            match serde_json::from_str::<Value>(stdout.trim()) {
                Ok(v) => {
                    if let Some(obj) = v.as_object() {
                        // 协议 1：显式 decision 字段。
                        if let Some(d) = obj.get("decision").and_then(Value::as_str) {
                            match d {
                                "continue" => HookDecision::Continue,
                                "rewrite" => obj
                                    .get("data")
                                    .cloned()
                                    .map(HookDecision::Rewrite)
                                    .unwrap_or(HookDecision::Continue),
                                "block" => HookDecision::Block {
                                    reason: obj
                                        .get("reason")
                                        .and_then(Value::as_str)
                                        .unwrap_or("blocked by hook")
                                        .to_string(),
                                },
                                _ => HookDecision::Continue,
                            }
                        } else {
                            // 协议 2：裸 JSON 对象 → 当作改写后的 data。
                            HookDecision::Rewrite(v)
                        }
                    } else {
                        HookDecision::Continue
                    }
                }
                Err(_) => HookDecision::Continue, // stdout 不是 JSON：当 Continue。
            }
        }
        2 => {
            // 退出 2：阻断（仅可阻断事件生效）。
            if event.is_blockable() {
                let reason = stdout.trim();
                let reason = if reason.is_empty() {
                    format!("blocked by hook (exit 2)")
                } else {
                    reason.to_string()
                };
                HookDecision::Block { reason }
            } else {
                tracing::warn!("hooks: exit 2 from non-blockable event {:?}; ignored", event);
                HookDecision::Continue
            }
        }
        _ => {
            // 其它非零：错误，按 Continue 处理（坏 hook 不杀会话）。
            tracing::warn!("hooks: non-zero exit {} (treating as Continue)", exit_code);
            HookDecision::Continue
        }
    }
}

/// 是否命中 matcher 过滤条件。
fn matches_filter(cfg: &HookConfig, payload: &HookPayload) -> bool {
    let Some(m) = &cfg.r#match else {
        return true;
    };
    // 仅 pre_tool_use / post_tool_use 关心工具名过滤。
    if let Some(tool_filter) = &m.tool {
        if let Some(tool) = payload.data.get("tool").and_then(Value::as_str) {
            return tool == tool_filter;
        }
        return false;
    }
    if let Some(glob_pat) = &m.tool_glob {
        if let Some(tool) = payload.data.get("tool").and_then(Value::as_str) {
            return glob_match(glob_pat, tool);
        }
        return false;
    }
    true
}

/// 极简 glob：支持 `*` / `?`（够用，hook matcher 不必上 regex 依赖）。
fn glob_match(pat: &str, s: &str) -> bool {
    // 把 glob 转成简单的递归匹配。
    fn rec(p: &[u8], t: &[u8]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            b'*' => {
                // `*` 匹配 0 或多个字符。
                if rec(&p[1..], t) {
                    return true;
                }
                if t.is_empty() {
                    return false;
                }
                rec(p, &t[1..])
            }
            b'?' => !t.is_empty() && rec(&p[1..], &t[1..]),
            c => !t.is_empty() && t[0] == c && rec(&p[1..], &t[1..]),
        }
    }
    rec(pat.as_bytes(), s.as_bytes())
}

fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn exit_zero_continues() {
        let cfg = HookConfig {
            event: HookEvent::PreToolUse,
            command: "true".into(),
            r#match: None,
            timeout_secs: 5,
            enabled: true,
        };
        let payload = HookPayload::new(HookEvent::PreToolUse, json!({"tool":"bash"}));
        let out = run_hook(&cfg, &payload).await;
        assert!(matches!(out.decision, HookDecision::Continue));
    }

    #[tokio::test]
    async fn exit_two_blocks_for_blockable_event() {
        let cfg = HookConfig {
            event: HookEvent::PreToolUse,
            command: "echo not allowed; exit 2".into(),
            r#match: None,
            timeout_secs: 5,
            enabled: true,
        };
        let payload = HookPayload::new(HookEvent::PreToolUse, json!({"tool":"bash"}));
        let out = run_hook(&cfg, &payload).await;
        match out.decision {
            HookDecision::Block { reason } => assert!(reason.contains("not allowed")),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exit_two_on_non_blockable_event_continues() {
        let cfg = HookConfig {
            event: HookEvent::PostToolUse,
            command: "exit 2".into(),
            r#match: None,
            timeout_secs: 5,
            enabled: true,
        };
        let payload = HookPayload::new(HookEvent::PostToolUse, json!({}));
        let out = run_hook(&cfg, &payload).await;
        assert!(matches!(out.decision, HookDecision::Continue));
    }

    #[tokio::test]
    async fn rewrite_via_stdout_json() {
        let cfg = HookConfig {
            event: HookEvent::UserPromptSubmit,
            command: r#"echo '{"prompt":"REWRITTEN"}'"#.into(),
            r#match: None,
            timeout_secs: 5,
            enabled: true,
        };
        let payload = HookPayload::new(HookEvent::UserPromptSubmit, json!({"prompt":"hi"}));
        let out = run_hook(&cfg, &payload).await;
        match out.decision {
            HookDecision::Rewrite(v) => {
                assert_eq!(v.get("prompt").and_then(|x| x.as_str()), Some("REWRITTEN"));
            }
            other => panic!("expected Rewrite, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn matcher_filters_by_tool_name() {
        let cfg = HookConfig {
            event: HookEvent::PreToolUse,
            command: "exit 2".into(),
            r#match: Some(HookMatcher {
                tool: Some("bash".to_string()),
                ..Default::default()
            }),
            timeout_secs: 5,
            enabled: true,
        };
        // 不匹配 → Continue。
        let payload = HookPayload::new(HookEvent::PreToolUse, json!({"tool":"read_file"}));
        let out = run_hook(&cfg, &payload).await;
        assert!(matches!(out.decision, HookDecision::Continue));
        // 匹配 → Block。
        let payload = HookPayload::new(HookEvent::PreToolUse, json!({"tool":"bash"}));
        let out = run_hook(&cfg, &payload).await;
        assert!(matches!(out.decision, HookDecision::Block { .. }));
    }

    #[test]
    fn glob_basic() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*_file", "read_file"));
        assert!(glob_match("*_file", "write_file"));
        assert!(!glob_match("*_file", "bash"));
        assert!(glob_match("?ash", "bash"));
        assert!(!glob_match("?ash", "trash"));
    }

    use super::super::types::HookMatcher;
}
