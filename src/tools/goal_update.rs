//! `goal_update`：模型向用户汇报长任务目标的结局。
//!
//! ## 为什么需要一个工具，而不是让模型在正文里写一句「目标完成」
//!
//! 自动继续（见 `goal.rs`）需要一个**可判定**的终止信号。从正文里捞关键词这条路
//! 经不起推敲：模型会复述这句话、会写成「还没完成」、会在解释规则时引用它，
//! 而这些都会让「目标到底完成没有」变成一次字符串赌博 —— 赌输的代价是模型
//! 一直在原地空转烧钱。工具调用是结构化的，`status` 是个枚举，不会有歧义。
//!
//! ## 为什么恒常注册、而不是只在有目标时挂上
//!
//! 工具表一变，请求前缀就变（见 `goal.rs` 模块头关于 cache 的说明），而且模型会
//! 重新打量整个工具列表。这里选择恒常注册，靠「没目标时调用直接报错」把路堵住 ——
//! 报错本身也是给模型的一句明确指令：现在没有目标，别干这件事。
//!
//! ## 与子 agent 的关系
//!
//! `bootstrap::build_tools` 里这个工具是在子 agent 的工具快照**之后**才挂上去的，
//! 子 agent 因此拿不到它 —— 否则子 agent 一次 `completed` 就能把父会话的目标标完成。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::goal::{GoalSignal, Signal};
use crate::tools::{Tool, ToolResult};

pub struct GoalUpdateTool {
    signal: Arc<GoalSignal>,
}

impl GoalUpdateTool {
    pub fn new(signal: Arc<GoalSignal>) -> Self {
        Self { signal }
    }
}

/// 把参数解析成一个结局。`status` 是**必填**的枚举，认不出来就报错而不是猜 ——
/// 猜错会让自动继续少停一轮或多停一轮，两种都很难查。
fn parse_signal(args: &Value) -> Result<Signal, String> {
    let status = args
        .get("status")
        .and_then(Value::as_str)
        .ok_or("缺少 status")?
        .trim()
        .to_ascii_lowercase();
    let note = args
        .get("note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    match status.as_str() {
        "completed" | "done" => Ok(Signal::Completed { note }),
        "blocked" => Ok(Signal::Blocked {
            note: note.ok_or("status = blocked 时必须用 note 说明卡在哪")?,
        }),
        "needs_user" | "needs-user" => Ok(Signal::NeedsUser {
            note: note.ok_or("status = needs_user 时必须用 note 说清要用户决定什么")?,
        }),
        other => Err(format!(
            "status 只能是 completed / blocked / needs_user，收到 `{other}`"
        )),
    }
}

/// 回灌给模型的一句话。它调这个工具是为了「把话交出去」，所以要确认收到了。
fn ack(s: &Signal) -> String {
    match s {
        Signal::Completed { .. } => "已记录：目标完成。自动继续已停止。".to_string(),
        Signal::Blocked { note } => {
            format!("已记录：目标被卡住（{note}）。自动继续已停止，用户会看到这条说明。")
        }
        Signal::NeedsUser { note } => {
            format!("已记录：需要用户拍板（{note}）。自动继续已停止，等用户回答。")
        }
    }
}

#[async_trait::async_trait]
impl Tool for GoalUpdateTool {
    fn name(&self) -> &str {
        "goal_update"
    }

    fn description(&self) -> &str {
        "汇报长任务目标（用户在会话里用 /goal 设定）的进展。\n\
         只在当前会话确实有推进中的目标时调用，而且只在两种时刻：\n\
         * 目标**已经完成** —— status = \"completed\"；\n\
         * 你推不动了 —— 被外部条件卡住（缺权限、缺依赖、上游报错）用 status = \"blocked\"，\
         必须由用户拍板才能继续（删文件、改接口约定、选方案）用 status = \"needs_user\"，\
         两种情况都要在 note 里说清是什么。\n\
         没有目标、或目标还在正常推进时**不要**调用它 —— 目标未完成而你自己停下，\
         carter 会自动再发起一轮「继续」，不需要你说。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "status": {
                    "type": "string",
                    "enum": ["completed", "blocked", "needs_user"],
                    "description": "completed = 目标已达成；blocked = 被外部条件挡住；needs_user = 必须用户决定"
                },
                "note": {
                    "type": "string",
                    "description": "一句话说明：完成了什么 / 卡在哪 / 要用户决定什么。blocked 与 needs_user 必填。"
                }
            },
            "required": ["status"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let signal = match parse_signal(&args) {
            Ok(s) => s,
            Err(e) => return ToolResult::err(format!("goal_update: {e}")),
        };
        if !self.signal.armed() {
            return ToolResult::err(
                "goal_update: 当前会话没有推进中的长任务目标（用户可以用 /goal 设定一个）。\
                 请直接继续回答或说明情况，不要调用这个工具。",
            );
        }
        let text = ack(&signal);
        self.signal.set(signal);
        ToolResult::ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(armed: bool) -> (GoalUpdateTool, Arc<GoalSignal>) {
        let signal = Arc::new(GoalSignal::default());
        signal.set_armed(armed);
        (GoalUpdateTool::new(signal.clone()), signal)
    }

    #[test]
    fn parses_the_three_outcomes() {
        assert_eq!(
            parse_signal(&json!({"status": "completed", "note": "做完了"})).unwrap(),
            Signal::Completed {
                note: Some("做完了".into())
            }
        );
        assert_eq!(
            parse_signal(&json!({"status": "blocked", "note": "没有网络"})).unwrap(),
            Signal::Blocked {
                note: "没有网络".into()
            }
        );
        assert_eq!(
            parse_signal(&json!({"status": "Needs_User", "note": "删不删？"})).unwrap(),
            Signal::NeedsUser {
                note: "删不删？".into()
            },
            "大小写与下划线不该影响识别"
        );
    }

    /// 认不出来的值必须报错而不是猜 —— 猜错的表现是自动继续多停/少停一轮，很难查。
    #[test]
    fn an_unknown_status_is_an_error_not_a_guess() {
        let e = parse_signal(&json!({"status": "maybe"})).unwrap_err();
        assert!(e.contains("maybe"), "{e}");
        assert!(parse_signal(&json!({})).is_err(), "status 是必填的");
    }

    /// 说「被卡住」却不说卡在哪，等于没汇报 —— 用户只会在目标条上看到一个没有内容的告警。
    #[test]
    fn stopping_outcomes_require_a_note() {
        assert!(parse_signal(&json!({"status": "blocked"})).is_err());
        assert!(parse_signal(&json!({"status": "needs_user", "note": "  "})).is_err());
        // 完成可以不写说明。
        assert!(parse_signal(&json!({"status": "completed"})).is_ok());
    }

    #[tokio::test]
    async fn a_completed_call_lands_in_the_shared_slot() {
        let (t, signal) = tool(true);
        let res = t
            .execute(json!({"status": "completed", "note": "告警清零"}))
            .await;
        assert!(res.ok, "{}", res.content);
        assert!(
            res.content.contains("停止"),
            "要告诉模型接下来会怎样：{}",
            res.content
        );
        assert_eq!(
            signal.take(),
            Some(Signal::Completed {
                note: Some("告警清零".into())
            })
        );
    }

    /// 恒常注册的代价就在这条：没有目标时必须被明确挡住，而不是默默「成功」。
    #[tokio::test]
    async fn without_a_goal_the_call_is_refused() {
        let (t, signal) = tool(false);
        let res = t.execute(json!({"status": "completed"})).await;
        assert!(!res.ok);
        assert!(
            res.content.contains("/goal"),
            "要把话说明白：{}",
            res.content
        );
        assert!(signal.take().is_none(), "被拒绝的调用不该留下信号");
    }
}
