//! llm_adapt 底座：把 carter 的请求交给它的协议 codec。

use std::sync::Arc;

use futures::StreamExt;
use llm_adapt_core::{ClientConfig, LLMClient, LLMClientBuilder, RequestHook};

use crate::error::{CarterError, Result};

use super::{Block, ChatRequest, EventStream, LlmProvider, WireRequest};

/// 用 llm_adapt 的客户端实现 [`LlmProvider`]。
///
/// 它只做三件事：收下请求、转给底座、把错误类型换掉。协议、重试、代理、
/// 超时、SSE 切帧、缓存断点全在底座里 —— 这一层刻意保持得很薄，
/// 因为它是 carter 唯一认识「LLM 是怎么调的」的地方。
pub struct WireProvider {
    client: LLMClient,
    /// 默认协议：请求没显式指定时用它。carter 的渠道配置决定。
    default_protocol: super::ProtocolId,
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
        hook: Option<Arc<dyn RequestHook>>,
    ) -> Result<Self> {
        let mut builder: LLMClientBuilder = LLMClient::builder(config);
        if let Some(h) = hook {
            builder = builder.hook(h);
        }
        let client = builder.build()?;
        Ok(Self {
            client,
            default_protocol,
        })
    }

    pub fn protocol(&self) -> &super::ProtocolId {
        &self.default_protocol
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
        let stream = self.client.chat_stream_ir(&protocol, &wire).await?;
        // 错误在流的边界上就地转成 carter 的类型，上层不必认识底座。
        Ok(Box::pin(stream.map(|item| item.map_err(CarterError::from))))
    }
}

/// 把请求里文本块中的 `[img:<hash>.<ext>]` 引用展开成真正的图片块。
///
/// 历史里存的是**引用 token**，不是图片字节 —— 否则一张图会在会话 JSONL 里以
/// base64 形态躺几百 KB，跑几轮就把文件撑到几十 MB。只有真要发出去时才去媒体库取字节。
///
/// 展开放在这一层，是因为媒体库属于 carter，底座不该认识它。展开后一个文本块会裂成
/// 「文本 / 图片 / 文本」这样的交替序列，**顺序不变** —— 图片在原文里的位置是有意义的。
fn expand_image_refs(wire: &mut WireRequest) {
    for msg in &mut wire.messages {
        if !msg
            .content
            .iter()
            .any(|b| matches!(b, Block::Text { text, .. } if text.contains("[img:")))
        {
            continue;
        }
        let mut rebuilt: Vec<Block> = Vec::with_capacity(msg.content.len());
        for block in std::mem::take(&mut msg.content) {
            let Block::Text { text, cache } = block else {
                rebuilt.push(block);
                continue;
            };
            let segs = crate::media::parse_segments(&text);
            let mut first_text = true;
            for seg in segs {
                match seg {
                    crate::media::Segment::Text(t) => {
                        if !t.is_empty() {
                            rebuilt.push(Block::Text {
                                text: t,
                                // 缓存断点跟着原来那个文本块走，只落在第一段上。
                                cache: if first_text { cache } else { None },
                            });
                            first_text = false;
                        }
                    }
                    crate::media::Segment::Image(rf) => match crate::media::read(&rf) {
                        Ok(bytes) => {
                            use base64::Engine;
                            rebuilt.push(Block::Image {
                                source: crate::provider::ImageSource::Base64 {
                                    media_type: rf.mime().to_string(),
                                    data: base64::engine::general_purpose::STANDARD.encode(bytes),
                                },
                                detail: None,
                                cache: None,
                            });
                            first_text = false;
                        }
                        Err(e) => {
                            // 读不到就把引用原样留成文本：让模型至少知道这里本该有张图，
                            // 也比静默少一段上下文强。
                            tracing::warn!("展开图片引用 {} 失败（{e}），保留文本引用", rf.token());
                            rebuilt.push(Block::text(rf.token()));
                            first_text = false;
                        }
                    },
                }
            }
        }
        msg.content = rebuilt;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Block, Role};

    #[test]
    fn image_refs_expand_into_blocks_in_place() {
        let rf = crate::media::put_bytes(b"\x89PNG\r\n\x1a\nfake", Some("png"))
            .expect("写媒体库");
        let mut wire = WireRequest::new("m");
        wire.messages = vec![crate::provider::WireMessage::new(
            Role::User,
            vec![Block::text(format!("看图 {} 再说", rf.token()))],
        )];

        expand_image_refs(&mut wire);
        let content = &wire.messages[0].content;
        assert_eq!(content.len(), 3, "应裂成 文本/图片/文本，顺序不变：{content:?}");
        assert!(matches!(&content[0], Block::Text { text, .. } if text == "看图 "));
        match &content[1] {
            Block::Image { source, .. } => {
                assert!(matches!(source, crate::provider::ImageSource::Base64 { .. }));
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
}
