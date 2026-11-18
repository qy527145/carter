//! llm_adapt 底座：把 carter 的请求交给它的协议 codec。

use std::sync::Arc;

use futures::StreamExt;
use llm_adapt_core::{ClientConfig, LLMClient, LLMClientBuilder, RequestHook};

use crate::error::{CarterError, Result};

use super::{Block, ChatRequest, EventStream, LlmProvider, WireRequest};

/// 一个渠道声明的服务端工具。
#[derive(Debug, Default, Clone)]
pub struct ServerToolConfig {
    /// 走内置表的名字（`web_search` 等）。
    pub names: Vec<String>,
    /// 原样下发的声明对象。
    pub raw_specs: Vec<serde_json::Value>,
    /// 单次响应内的调用次数上限。
    pub max_uses: Option<u32>,
}

/// 用 llm_adapt 的客户端实现 [`LlmProvider`]。
///
/// 它只做三件事：收下请求、转给底座、把错误类型换掉。协议、重试、代理、
/// 超时、SSE 切帧、缓存断点全在底座里 —— 这一层刻意保持得很薄，
/// 因为它是 carter 唯一认识「LLM 是怎么调的」的地方。
pub struct WireProvider {
    client: LLMClient,
    /// 默认协议：请求没显式指定时用它。carter 的渠道配置决定。
    default_protocol: super::ProtocolId,
    /// 该渠道声明的服务端工具（厂商自己执行的，如 web_search）。
    server_tools: Vec<super::ToolSpec>,
    /// 声明不出去的名字：该协议没有对应物、或压根不认识。
    /// 启动时会报给用户，不含糊过去。
    unsupported_server_tools: Vec<String>,
}

impl std::fmt::Debug for WireProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireProvider")
            .field("protocol", &self.default_protocol)
            .finish_non_exhaustive()
    }
}

impl WireProvider {
    /// 从底座配置构造。`default_protocol` 是该渠道声明支持的线协议。
    pub fn new(
        config: ClientConfig,
        default_protocol: super::ProtocolId,
        server_tools: ServerToolConfig,
        hook: Option<Arc<dyn RequestHook>>,
        extra_hooks: Vec<Arc<dyn RequestHook>>,
    ) -> Result<Self> {
        // 服务端工具按渠道声明的协议解析。自定义协议没有内置表，只能靠原样下发。
        let (mut specs, mut dropped) = match default_protocol.protocol() {
            Some(proto) => {
                llm_adapt_core::resolve_specs(proto, &server_tools.names, server_tools.max_uses)
            }
            None => (Vec::new(), server_tools.names.clone()),
        };
        for raw in &server_tools.raw_specs {
            match llm_adapt_core::spec_from_raw(raw) {
                Some(spec) => specs.push(spec),
                None => dropped.push("(缺少 type 字段的 server_tool_specs 条目)".to_string()),
            }
        }
        if !dropped.is_empty() {
            tracing::warn!(
                protocol = %default_protocol,
                dropped = ?dropped,
                "这些服务端工具声明不出去：该协议没有对应物或名字不认识"
            );
        }

        let mut builder: LLMClientBuilder = LLMClient::builder(config);
        if let Some(h) = hook {
            builder = builder.hook(h);
        }
        // 宿主（GUI）自己的钩子：抓包、指标之类。
        for h in extra_hooks {
            builder = builder.hook(h);
        }
        let client = builder.build()?;
        Ok(Self {
            client,
            default_protocol,
            server_tools: specs,
            unsupported_server_tools: dropped,
        })
    }

    pub fn protocol(&self) -> &super::ProtocolId {
        &self.default_protocol
    }

    /// 声明不出去的服务端工具名字。空 = 全部生效。
    pub fn unsupported_server_tools(&self) -> &[String] {
        &self.unsupported_server_tools
    }
}

#[async_trait::async_trait]
impl LlmProvider for WireProvider {
    async fn stream(&self, req: ChatRequest) -> Result<EventStream> {
        // 请求没指定协议就用渠道默认的；指定了就听请求的（同一进程里可以混用渠道）。
        let protocol = req
            .protocol
            .unwrap_or_else(|| self.default_protocol.clone());
        let mut wire = req.wire;
        expand_image_refs(&mut wire);
        // 服务端工具是**渠道能力**，不是本地工具表的一部分 ——
        // 由这里并进去，而不是让 ToolRegistry 产出它们（那会让执行层看见一堆
        // 本地根本没有实现的名字）。
        if !self.server_tools.is_empty() {
            wire.tools.extend(self.server_tools.iter().cloned());
        }
        let stream = self.client.chat_stream_ir(&protocol, &wire).await?;
        // 错误在流的边界上就地转成 carter 的类型，上层不必认识底座。
        Ok(Box::pin(stream.map(|item| item.map_err(CarterError::from))))
    }
}

/// 把请求里的 `[img:<hash>.<ext>]` 引用展开成真正的图片块。
///
/// 历史里存的是**引用 token**，不是图片字节 —— 否则一张图会在会话 JSONL 里以
/// base64 形态躺几百 KB，跑几轮就把文件撑到几十 MB。只有真要发出去时才去媒体库取字节。
///
/// 展开放在这一层，是因为媒体库属于 carter，底座不该认识它。两条规则：
///
/// 1. **只展开 user 轮。** assistant 的正文里出现 `[img:…]`，是模型在**提到**那个
///    引用（「工具只回给我一个 `[img:…]` 标记」），不是它要发图 —— 而且 Anthropic
///    明确拒绝 assistant 轮里的 image 块（400 `'image' blocks are not permitted
///    within assistant turns`），一旦那句话落进历史，整个会话从此每次请求都炸。
///    所以 assistant 轮的引用原样留作文本。
/// 2. **工具结果也要展开。** `read_file` 读图片回的就是一条带引用的文本结果，
///    不展开模型永远只能看到一串字面量。展开后 `ToolResultContent::Text` 变成
///    `Blocks`，图片以 data URL 放进去：Anthropic 原生收，OpenAI 一类协议由底座
///    降级成文本并告警 —— 至少不比现在差。
///
/// 展开后一个文本块会裂成「文本 / 图片 / 文本」这样的交替序列，**顺序不变** ——
/// 图片在原文里的位置是有意义的。
fn expand_image_refs(wire: &mut WireRequest) {
    for msg in &mut wire.messages {
        if msg.role != super::Role::User {
            continue;
        }
        let has_ref = msg.content.iter().any(|b| match b {
            Block::Text { text, .. } => text.contains("[img:"),
            Block::ToolResult {
                content: super::ToolResultContent::Text { text },
                ..
            } => text.contains("[img:"),
            _ => false,
        });
        if !has_ref {
            continue;
        }
        let mut rebuilt: Vec<Block> = Vec::with_capacity(msg.content.len());
        for block in std::mem::take(&mut msg.content) {
            match block {
                Block::Text { text, cache } => expand_text_block(&text, cache, &mut rebuilt),
                Block::ToolResult {
                    tool_use_id,
                    content: super::ToolResultContent::Text { text },
                    is_error,
                    cache,
                } if text.contains("[img:") => rebuilt.push(Block::ToolResult {
                    tool_use_id,
                    content: expand_tool_result_text(&text),
                    is_error,
                    cache,
                }),
                other => rebuilt.push(other),
            }
        }
        msg.content = rebuilt;
    }
}

/// 一个 user 文本块 → 「文本 / 图片 / 文本」序列，追加进 `out`。
fn expand_text_block(text: &str, cache: Option<super::CacheMarker>, out: &mut Vec<Block>) {
    let mut first_text = true;
    for seg in crate::media::parse_segments(text) {
        match seg {
            crate::media::Segment::Text(t) => {
                if !t.is_empty() {
                    out.push(Block::Text {
                        text: t,
                        // 缓存断点跟着原来那个文本块走，只落在第一段上。
                        cache: if first_text { cache } else { None },
                    });
                    first_text = false;
                }
            }
            crate::media::Segment::Image(rf) => match load_image_base64(&rf) {
                Some(data) => {
                    out.push(Block::Image {
                        source: crate::provider::ImageSource::Base64 {
                            media_type: rf.mime().to_string(),
                            data,
                        },
                        detail: None,
                        cache: None,
                    });
                    first_text = false;
                }
                None => {
                    // 读不到就把引用原样留成文本：让模型至少知道这里本该有张图，
                    // 也比静默少一段上下文强。
                    out.push(Block::text(rf.token()));
                    first_text = false;
                }
            },
        }
    }
}

/// 工具结果的文本 → 多模态块列表。读不到的图同样退回文本引用。
fn expand_tool_result_text(text: &str) -> super::ToolResultContent {
    use super::ToolResultBlock;
    let mut blocks = Vec::new();
    for seg in crate::media::parse_segments(text) {
        match seg {
            crate::media::Segment::Text(t) => {
                if !t.is_empty() {
                    blocks.push(ToolResultBlock::Text { text: t });
                }
            }
            crate::media::Segment::Image(rf) => match load_image_base64(&rf) {
                Some(data) => blocks.push(ToolResultBlock::Image {
                    url: format!("data:{};base64,{data}", rf.mime()),
                    detail: None,
                }),
                None => blocks.push(ToolResultBlock::Text { text: rf.token() }),
            },
        }
    }
    super::ToolResultContent::Blocks { blocks }
}

/// 从媒体库取字节并 base64。失败只记一条告警，由调用方决定怎么退化。
fn load_image_base64(rf: &crate::media::ImageRef) -> Option<String> {
    use base64::Engine;
    match crate::media::read(rf) {
        Ok(bytes) => Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
        Err(e) => {
            tracing::warn!("展开图片引用 {} 失败（{e}），保留文本引用", rf.token());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Block, Role};

    #[test]
    fn image_refs_expand_into_blocks_in_place() {
        let rf = crate::media::put_bytes(b"\x89PNG\r\n\x1a\nfake", Some("png")).expect("写媒体库");
        let mut wire = WireRequest::new("m");
        wire.messages = vec![crate::provider::WireMessage::new(
            Role::User,
            vec![Block::text(format!("看图 {} 再说", rf.token()))],
        )];

        expand_image_refs(&mut wire);
        let content = &wire.messages[0].content;
        assert_eq!(
            content.len(),
            3,
            "应裂成 文本/图片/文本，顺序不变：{content:?}"
        );
        assert!(matches!(&content[0], Block::Text { text, .. } if text == "看图 "));
        match &content[1] {
            Block::Image { source, .. } => {
                assert!(matches!(
                    source,
                    crate::provider::ImageSource::Base64 { .. }
                ));
            }
            other => panic!("中间应是图片块，得到 {other:?}"),
        }
        assert!(matches!(&content[2], Block::Text { text, .. } if text == " 再说"));
    }

    #[test]
    fn text_without_refs_is_left_alone() {
        let mut wire = WireRequest::new("m");
        let original = vec![Block::text("普通的文本"), Block::text("没有图片")];
        wire.messages = vec![crate::provider::WireMessage::new(
            Role::User,
            original.clone(),
        )];
        expand_image_refs(&mut wire);
        assert_eq!(wire.messages[0].content, original);
    }

    /// 引用指向的文件不在时，宁可留一段「这里本该有张图」的文本，
    /// 也不要静默少掉上下文。
    #[test]
    fn missing_blob_keeps_the_text_ref() {
        let mut wire = WireRequest::new("m");
        wire.messages = vec![crate::provider::WireMessage::new(
            Role::User,
            vec![Block::text("[img:deadbeef.png]")],
        )];
        expand_image_refs(&mut wire);
        let content = &wire.messages[0].content;
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].as_text(), Some("[img:deadbeef.png]"));
    }

    /// assistant 提到 `[img:…]` 只是在说话，不是在发图。展开了就是 Anthropic 的
    /// 400 `'image' blocks are not permitted within assistant turns`，而且这句话一旦
    /// 进了历史，之后整个会话都发不出去。
    #[test]
    fn assistant_turns_are_never_expanded() {
        let rf =
            crate::media::put_bytes(b"\x89PNG\r\n\x1a\nfake-a", Some("png")).expect("写媒体库");
        let quoted = format!("工具只回给我一个 `{}` 引用", rf.token());
        let mut wire = WireRequest::new("m");
        wire.messages = vec![crate::provider::WireMessage::new(
            Role::Assistant,
            vec![Block::text(quoted.clone())],
        )];
        expand_image_refs(&mut wire);
        let content = &wire.messages[0].content;
        assert_eq!(content.len(), 1, "{content:?}");
        assert_eq!(content[0].as_text(), Some(quoted.as_str()));
    }

    /// `read_file` 读图回的是带引用的文本工具结果：得变成多模态块，模型才真看得到。
    #[test]
    fn tool_result_text_refs_expand_into_blocks() {
        use crate::provider::{ToolResultBlock, ToolResultContent};
        let rf =
            crate::media::put_bytes(b"\x89PNG\r\n\x1a\nfake-t", Some("png")).expect("写媒体库");
        let mut wire = WireRequest::new("m");
        wire.messages = vec![crate::provider::WireMessage::new(
            Role::User,
            vec![Block::ToolResult {
                tool_use_id: "t1".into(),
                content: ToolResultContent::text(format!(
                    "{}\n[image stored: 12 bytes, image/png mime]",
                    rf.token()
                )),
                is_error: false,
                cache: None,
            }],
        )];
        expand_image_refs(&mut wire);
        let Block::ToolResult {
            tool_use_id,
            content,
            ..
        } = &wire.messages[0].content[0]
        else {
            panic!("工具结果块应原位保留：{:?}", wire.messages[0].content);
        };
        assert_eq!(tool_use_id, "t1");
        let ToolResultContent::Blocks { blocks } = content else {
            panic!("应展开成多模态块：{content:?}");
        };
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        match &blocks[0] {
            ToolResultBlock::Image { url, .. } => {
                assert!(url.starts_with("data:image/png;base64,"), "{url}");
            }
            other => panic!("第一块应是图片，得到 {other:?}"),
        }
        assert!(matches!(
            &blocks[1],
            ToolResultBlock::Text { text } if text.contains("[image stored:")
        ));
    }

    /// 工具结果没有引用时一个字节都不动 —— `Text` 形态在 wire 上是裸字符串，
    /// 无故改成块数组会让缓存失效。
    #[test]
    fn tool_result_without_refs_is_left_alone() {
        use crate::provider::ToolResultContent;
        let mut wire = WireRequest::new("m");
        let block = Block::ToolResult {
            tool_use_id: "t2".into(),
            content: ToolResultContent::text("plain output"),
            is_error: false,
            cache: None,
        };
        wire.messages = vec![crate::provider::WireMessage::new(
            Role::User,
            vec![block.clone()],
        )];
        expand_image_refs(&mut wire);
        assert_eq!(wire.messages[0].content, vec![block]);
    }
}
