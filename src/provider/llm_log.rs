//! 请求报文日志（`[debug] log_requests`）。
//!
//! 实现成底座的 [`RequestHook`] 而不是塞进 provider：hook 拿到的 `HttpRequest.body`
//! 就是最终要发出去的**字节**。旧底座（genai）只能给出归一化视图，排障时经常对不上
//! 真实报文 —— 现在记下来的是原名原样的东西。
//!
//! 只记请求与（非流式的）响应头/体。**流式响应的逐帧不在这里** ——
//! 逐块往按天滚的文件里追加只会得到一堆碎片；那部分由客户端的捕获面板负责，
//! 它按会话分文件、按交换归拢，并带时序。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use llm_adapt_core::{HttpRequest, RawHttpResponse, RequestHook};
use serde_json::{Value, json};

/// 落盘目录由 `[debug] llm_log_dir` 决定，默认 `~/.carter/debug/llm_log`。
pub struct LlmLogHook {
    dir: PathBuf,
    /// 请求 / 响应配对的序号。hook 是同步的、没有请求上下文，
    /// 只能靠这个把成对的两条记录串起来。
    seq: AtomicU64,
}

impl LlmLogHook {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            seq: AtomicU64::new(0),
        }
    }

    fn append(&self, kind: &str, seq: u64, payload: Value) {
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let now = crate::session::now_ms();
        let path = self
            .dir
            .join(format!("{}.jsonl", crate::session::date_utc(now)));
        let line = json!({
            "ts": now,
            "seq": seq,
            "kind": kind,
            "data": payload,
        });
        let Ok(mut s) = serde_json::to_string(&line) else {
            return;
        };
        s.push('\n');
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(s.as_bytes());
        }
    }
}

impl RequestHook for LlmLogHook {
    fn before_request(&self, req: &HttpRequest) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.append(
            "request",
            seq,
            json!({
                "correlation": req.correlation,
                "method": format!("{:?}", req.method),
                "url": req.url,
                "headers": redact(&req.headers),
                "body": parse_body(&req.body),
            }),
        );
    }

    fn after_raw_response(&self, req: &HttpRequest, resp: &RawHttpResponse) {
        // 配对键是「请求的关联标签 + URL」。钩子会同时服务并发请求，
        // 靠「上一条请求」这种顺序假设配对，并发一上来就错。
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.append(
            "response",
            seq,
            json!({
                "correlation": req.correlation,
                "url": req.url,
                "status": resp.status,
                "headers": redact(&resp.headers),
                "body": parse_body(&resp.body),
            }),
        );
    }

    // 刻意不实现 `on_stream_chunk`：流式响应体逐块来，往这个按天滚的文件里
    // 逐块追加会把它变成一堆碎片。要看 SSE 逐帧请用客户端的捕获面板 ——
    // 那里按会话分文件、并按交换归拢。
}

/// 密钥落盘前脱敏。日志是给人看的，不该顺手把 key 抄一份到磁盘上。
fn redact(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(k, v)| {
            let lower = k.to_ascii_lowercase();
            let sensitive = lower == "authorization"
                || lower == "x-api-key"
                || lower.contains("api-key")
                || lower.contains("token");
            if sensitive {
                (k.clone(), "***".to_string())
            } else {
                (k.clone(), v.clone())
            }
        })
        .collect()
}

/// 能当 JSON 解析就当 JSON（便于阅读与 diff），否则退回字符串。
fn parse_body(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        return Value::Null;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string())),
        Err(_) => Value::String(format!("<{} 字节的二进制>", bytes.len())),
    }
}
