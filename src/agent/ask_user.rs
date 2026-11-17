//! `ask_user_question`：一次性问**一组**问题，支持多选。
//!
//! ## 为什么不是「一次一个问题」
//!
//! 一个问题、一个答案的 schema 想收集三件事就得调三次工具、来回三轮，而每一轮都要
//! 走一遍完整的模型请求。而且选项只有标签、没有说明，用户看到「选项 A」不知道它
//! 意味着什么。
//!
//! 这里改成一次问一组：每个问题可以带说明的选项、可以多选、可以带一个短标签。
//! 用户填完一次性提交，模型拿到的是**一份完整答案**而不是挤牙膏的结果。
//!
//! ## 兼容旧的调用形态
//!
//! 模型见过的语料里 `ask_user_question` 大多长成 `{question, options:[string]}`。
//! 只认新 schema 的话，那些调用会得到一个「参数错误」，而模型多半会**重试同样的
//! 形状** —— 白烧几轮。所以这里两种形态都收。
//!
//! ## 宿主怎么接
//!
//! 调用 ui 通道发 `UiEvent::AskUser`，配上 oneshot Sender；宿主弹窗、用户作答后把
//! 「第 i 个问题 → 选中的若干项」发回来。oneshot 模式（无 UI）由 `StdoutSink` 自动
//! 每题选第一项作答，避免一次性执行被卡死。

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::tools::{Tool, ToolResult};

use super::ui::UiEvent;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// 一个选项：文字 + 可选的一句说明（界面上作次要说明显示）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskOption {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// 一个问题。`header` 是极短标签，界面上当标题用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestion {
    pub question: String,
    #[serde(default)]
    pub header: Option<String>,
    #[serde(default)]
    pub multi_select: bool,
    #[serde(default)]
    pub options: Vec<AskOption>,
}

/// 工具实例：持有一个把事件发到宿主的 sender。
pub struct AskUserQuestionTool {
    ui_tx: UnboundedSender<UiEvent>,
}

impl AskUserQuestionTool {
    pub fn new(ui_tx: UnboundedSender<UiEvent>) -> Self {
        Self { ui_tx }
    }
}

/// 把参数解析成问题列表。新旧两种形态都收，见模块头。
pub fn parse_questions(args: &Value) -> Result<Vec<AskQuestion>, String> {
    // 形态一（新）：{ questions: [ { question, header?, multi_select?, options? } ] }
    if let Some(arr) = args.get("questions").and_then(Value::as_array) {
        let mut out = Vec::new();
        for (i, q) in arr.iter().enumerate() {
            out.push(parse_one(q).map_err(|e| format!("第 {} 个问题：{e}", i + 1))?);
        }
        if out.is_empty() {
            return Err("questions 是空的".into());
        }
        return Ok(out);
    }

    // 形态二（旧）：{ question, options: [string] }
    if args.get("question").is_some() {
        return Ok(vec![parse_one(args)?]);
    }

    Err("缺少 questions（要问的问题列表）".into())
}

fn parse_one(q: &Value) -> Result<AskQuestion, String> {
    let question = q
        .get("question")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or("缺少 question")?
        .to_string();

    let options: Vec<AskOption> = q
        .get("options")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|o| match o {
                    // 新形态：{ label, description }
                    Value::Object(_) => o.get("label").and_then(Value::as_str).map(|l| AskOption {
                        label: l.to_string(),
                        description: o
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    }),
                    // 旧形态：裸字符串
                    Value::String(s) => Some(AskOption {
                        label: s.clone(),
                        description: None,
                    }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(AskQuestion {
        question,
        header: q.get("header").and_then(Value::as_str).map(str::to_string),
        multi_select: q
            .get("multi_select")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        options,
    })
}

/// 把答案渲染成回灌给模型的一段文本。
///
/// 多个问题时必须把问题也带上 —— 否则模型拿到的是一串没有主语的答案，
/// 上下文一长就分不清哪句答的是哪问。
pub fn render_answers(pairs: &[(AskQuestion, Vec<String>)]) -> String {
    if pairs.len() == 1 {
        let (q, a) = &pairs[0];
        if a.is_empty() {
            return format!("问题：{}\n用户没有作答。", q.question);
        }
        return format!("问题：{}\n用户选择：{}", q.question, a.join("、"));
    }
    let mut out = String::from("用户对以下问题作出了回答：\n");
    for (i, (q, a)) in pairs.iter().enumerate() {
        let answer = if a.is_empty() {
            "（未作答）".to_string()
        } else {
            a.join("、")
        };
        let head = q.header.as_deref().unwrap_or("");
        out.push_str(&format!(
            "{}. {}：{}\n   答：{}\n",
            i + 1,
            if head.is_empty() { "问题" } else { head },
            q.question,
            answer
        ));
    }
    out
}

#[async_trait::async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "ask_user_question"
    }

    fn description(&self) -> &str {
        "向用户提问并等待回答。**一次可以问多个问题**，每个问题可选多选。\
         适合：需求有歧义、需要用户在方案间取舍、敏感操作前确认。\n\
         一次问完比分几轮问好 —— 每多一轮就多一次完整的模型请求。\n\
         用户也可能不作答（跳过），这时要能继续往下走。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "要问的问题（1-4 个）。一次问完，不要分多轮。",
                    "minItems": 1,
                    "maxItems": 4,
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": {
                                "type": "string",
                                "description": "问题本身，一句话说清楚"
                            },
                            "header": {
                                "type": "string",
                                "description": "极短标签（最多 12 字），界面上当标题用，如「数据库」「部署方式」"
                            },
                            "multi_select": {
                                "type": "boolean",
                                "description": "是否允许多选（缺省 false = 单选）"
                            },
                            "options": {
                                "type": "array",
                                "description": "候选项。给了就渲染成选项按钮，用户仍可自由输入。",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string", "description": "选项文字" },
                                        "description": {
                                            "type": "string",
                                            "description": "这个选项意味着什么（可选，短）"
                                        }
                                    },
                                    "required": ["label"]
                                }
                            }
                        },
                        "required": ["question"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    async fn execute(&self, args: Value) -> ToolResult {
        let questions = match parse_questions(&args) {
            Ok(q) => q,
            Err(e) => return ToolResult::err(format!("ask_user_question: {e}")),
        };

        let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
        let (reply, rx) = oneshot::channel::<Vec<Vec<String>>>();
        let event = UiEvent::AskUser {
            id,
            questions: questions.clone(),
            response_tx: std::sync::Arc::new(std::sync::Mutex::new(Some(reply))),
        };
        if self.ui_tx.send(event).is_err() {
            return ToolResult::err("ask_user_question: UI channel closed");
        }

        // `[]` = 用户跳过了。不是错误 —— 模型应当能继续往下走。
        let answers = match rx.await {
            Ok(a) => a,
            Err(_) => return ToolResult::err("ask_user_question: oneshot dropped (UI gone?)"),
        };
        let pairs: Vec<(AskQuestion, Vec<String>)> = questions
            .into_iter()
            .enumerate()
            .map(|(i, q)| {
                let a = answers.get(i).cloned().unwrap_or_default();
                (q, a)
            })
            .collect();

        let text = render_answers(&pairs);
        if pairs.iter().all(|(_, a)| a.is_empty()) {
            return ToolResult::ok(format!("{text}\n（用户跳过了，请按最合理的默认继续）"));
        }
        ToolResult::ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_new_multi_question_shape() {
        let args = json!({
            "questions": [
                { "question": "用哪个数据库？", "header": "数据库", "multi_select": false,
                  "options": [{"label": "Postgres", "description": "已有实例"}] },
                { "question": "要哪些功能？", "multi_select": true,
                  "options": [{"label": "登录"}, {"label": "导出"}] }
            ]
        });
        let qs = parse_questions(&args).unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].header.as_deref(), Some("数据库"));
        assert!(!qs[0].multi_select);
        assert!(qs[1].multi_select);
        assert_eq!(qs[1].options.len(), 2);
        assert_eq!(qs[0].options[0].description.as_deref(), Some("已有实例"));
    }

    /// 模型见过的语料里这个工具大多是旧形态。只认新 schema 的话，它会拿到参数错误、
    /// 然后**重试同样的形状**，白烧几轮。
    #[test]
    fn the_old_single_question_shape_still_works() {
        let args = json!({ "question": "继续吗？", "options": ["继续", "停下"] });
        let qs = parse_questions(&args).unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].question, "继续吗？");
        assert_eq!(qs[0].options.len(), 2);
        assert!(qs[0].options[0].description.is_none());
    }

    #[test]
    fn missing_questions_is_a_clear_error() {
        assert!(parse_questions(&json!({})).is_err());
        assert!(parse_questions(&json!({"questions": []})).is_err());
    }

    #[test]
    fn a_bad_entry_names_which_question_is_wrong() {
        let args = json!({ "questions": [{ "question": "好的" }, { "header": "缺了 question" }] });
        let err = parse_questions(&args).unwrap_err();
        assert!(err.contains("第 2 个问题"), "{err}");
    }

    /// 多个问题的答案必须带上问题本身，否则模型拿到的是一串没有主语的答案。
    #[test]
    fn multiple_answers_are_rendered_with_their_questions() {
        let pairs = vec![
            (
                AskQuestion {
                    question: "用哪个数据库？".into(),
                    header: Some("数据库".into()),
                    multi_select: false,
                    options: vec![],
                },
                vec!["Postgres".to_string()],
            ),
            (
                AskQuestion {
                    question: "要哪些功能？".into(),
                    header: None,
                    multi_select: true,
                    options: vec![],
                },
                vec!["登录".to_string(), "导出".to_string()],
            ),
        ];
        let out = render_answers(&pairs);
        assert!(out.contains("用哪个数据库？"), "{out}");
        assert!(out.contains("Postgres"), "{out}");
        assert!(out.contains("登录、导出"), "多选要连起来：{out}");
    }

    #[test]
    fn a_skipped_question_says_so_instead_of_becoming_empty_text() {
        let pairs = vec![(
            AskQuestion {
                question: "继续吗？".into(),
                header: None,
                multi_select: false,
                options: vec![],
            },
            vec![],
        )];
        let out = render_answers(&pairs);
        assert!(out.contains("没有作答"), "{out}");
    }

    #[tokio::test]
    async fn emits_event_and_waits_for_the_group_answer() {
        let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = AskUserQuestionTool::new(ui_tx);
        tokio::spawn(async move {
            if let Some(UiEvent::AskUser { response_tx, .. }) = ui_rx.recv().await {
                if let Ok(mut guard) = response_tx.lock() {
                    if let Some(tx) = guard.take() {
                        let _ = tx.send(vec![vec!["Postgres".to_string()], vec![]]);
                    }
                }
            }
        });
        let res = tool
            .execute(json!({
                "questions": [
                    { "question": "用哪个数据库？", "options": [{"label":"Postgres"}] },
                    { "question": "还要什么？" }
                ]
            }))
            .await;
        assert!(res.ok, "{}", res.content);
        assert!(res.content.contains("Postgres"), "{}", res.content);
        assert!(res.content.contains("未作答"), "{}", res.content);
    }
}
