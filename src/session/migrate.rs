//! 会话格式迁移。
//!
//! **只读迁移**：打开老文件时在内存里转换，不原地改写。下一次落盘写的是新格式，
//! 老文件原样留着 —— 迁移代码写错时还有退路，而「原地改写会话历史」是不可逆的。
//!
//! ## v1 → v2
//!
//! 消息模型从纯文本枚举换成 IR（[`WireMessage`]）。v1 的消息是外部标签形态
//! （`Message` 当时是普通 enum，没有 `#[serde(tag)]`）：
//! `{"User":"文本"}`、`{"Assistant":"文本"}`、`{"ToolCalls":[…]}`、`{"Tool":{…}}`。
//!
//! 迁移是**有损的**，且损失是 v1 本来就有的：v1 的纯文本消息存不住思考签名、
//! 工具结果里的图片、认不出的服务端工具块。这里只是把当时能存下来的东西忠实地
//! 搬进新模型，不会凭空补出信息。

use serde::Deserialize;
use serde_json::Value;

use crate::provider::{Block, Role, ToolResultContent, WireMessage};

/// v1 的消息形态。只读，永远不再写出去。
#[derive(Debug, Deserialize)]
enum MessageV1 {
    System(String),
    User(String),
    Assistant(String),
    ToolCalls(Vec<ToolCallV1>),
    Tool { call_id: String, content: String },
}

#[derive(Debug, Deserialize)]
struct ToolCallV1 {
    id: String,
    name: String,
    #[serde(default)]
    args: Value,
}

impl MessageV1 {
    fn into_ir(self) -> WireMessage {
        match self {
            // v1 的 `System` 变体从未被生产路径写过（当时就注明「预留给 system prompt 注入」），
            // 但这里仍然转出来而不是丢掉 —— 迁移不该悄悄减少内容。
            MessageV1::System(s) => WireMessage::new(Role::System, vec![Block::text(s)]),
            MessageV1::User(s) => WireMessage::user_text(s),
            MessageV1::Assistant(s) => WireMessage::new(Role::Assistant, vec![Block::text(s)]),
            MessageV1::ToolCalls(calls) => WireMessage::new(
                Role::Assistant,
                calls
                    .into_iter()
                    .map(|c| Block::ToolUse {
                        id: c.id,
                        name: c.name,
                        input: c.args,
                        cache: None,
                    })
                    .collect(),
            ),
            MessageV1::Tool { call_id, content } => WireMessage::new(
                Role::User,
                vec![Block::ToolResult {
                    tool_use_id: call_id,
                    content: ToolResultContent::Text { text: content },
                    is_error: false,
                    cache: None,
                }],
            ),
        }
    }
}

/// 试着把一行 v1 记录升级到当前版本。`None` 表示这行不是 v1 消息记录
/// （调用方据此走原有的解析路径）。
///
/// 只处理 `message` 与 `compacted` 两种 —— v1 与 v2 的差异只在这两处的载荷里，
/// 其余记录（meta / title / todo / checkpoint）形状没变。
pub fn upgrade_v1_line(line: &str) -> Option<Value> {
    let mut v: Value = serde_json::from_str(line).ok()?;
    match v.get("type").and_then(Value::as_str)? {
        "message" => {
            let payload = v.get("payload")?.clone();
            let msg: MessageV1 = serde_json::from_value(payload).ok()?;
            v["payload"] = serde_json::to_value(msg.into_ir()).ok()?;
        }
        "compacted" => {
            let items = v.pointer("/payload/messages")?.as_array()?.clone();
            let mut converted = Vec::with_capacity(items.len());
            for item in items {
                let msg: MessageV1 = serde_json::from_value(item).ok()?;
                converted.push(serde_json::to_value(msg.into_ir()).ok()?);
            }
            v["payload"]["messages"] = Value::Array(converted);
        }
        _ => return None,
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrades_v1_text_messages() {
        let up = upgrade_v1_line(r#"{"ts":1,"type":"message","payload":{"User":"hi"}}"#).unwrap();
        assert_eq!(up["payload"]["role"], "user");
        assert_eq!(up["payload"]["content"][0]["type"], "text");
        assert_eq!(up["payload"]["content"][0]["text"], "hi");
    }

    #[test]
    fn upgrades_v1_tool_calls_and_results() {
        let calls = upgrade_v1_line(
            r#"{"ts":1,"type":"message","payload":{"ToolCalls":[{"id":"t1","name":"f","args":{"a":1}}]}}"#,
        )
        .unwrap();
        assert_eq!(calls["payload"]["role"], "assistant");
        assert_eq!(calls["payload"]["content"][0]["type"], "tool_use");
        assert_eq!(calls["payload"]["content"][0]["input"]["a"], 1);

        let result = upgrade_v1_line(
            r#"{"ts":2,"type":"message","payload":{"Tool":{"call_id":"t1","content":"out"}}}"#,
        )
        .unwrap();
        assert_eq!(result["payload"]["role"], "user");
        assert_eq!(result["payload"]["content"][0]["type"], "tool_result");
        assert_eq!(result["payload"]["content"][0]["tool_use_id"], "t1");
    }

    #[test]
    fn upgrades_compacted_snapshot() {
        let up = upgrade_v1_line(
            r#"{"ts":1,"type":"compacted","payload":{"tier":"L3","messages":[{"User":"q"},{"Assistant":"a"}]}}"#,
        )
        .unwrap();
        let msgs = up["payload"]["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["role"], "assistant");
    }

    /// 已经不是 v1 的行必须返回 None，否则会把 v2 内容二次转换。
    #[test]
    fn returns_none_for_non_v1_records() {
        assert!(upgrade_v1_line(r#"{"ts":1,"type":"title","payload":{"title":"x"}}"#).is_none());
        // v2 的消息载荷是带 type 标签的对象，解析不成 MessageV1。
        assert!(
            upgrade_v1_line(r#"{"ts":1,"type":"message","payload":{"role":"user","content":[]}}"#)
                .is_none()
        );
    }
}
