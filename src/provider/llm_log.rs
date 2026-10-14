//! 请求报文日志（`[debug] log_requests`）。
//!
//! 实现成底座的 [`RequestHook`] 而不是塞进 provider：hook 拿到的 `HttpRequest.body`
//! 就是最终要发出去的**字节**。旧底座（genai）只能给出归一化视图，排障时经常对不上
//! 真实报文 —— 现在记下来的是原名原样的东西。
//!
//! 只记请求与（非流式的）响应头/体。**流式响应的逐帧**要等 M5 的捕获层，
//! 那时会连 SSE 帧与耗时一起落到 SQLite，供对话里的「查看原始报文」用。

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
                "method": format!("{:?}", req.method),
                "url": req.url,
                "headers": redact(&req.headers),
                "body": parse_body(&req.body),
            }),
        );
    }

    fn after_raw_response(&self, resp: &RawHttpResponse) {
        // 用当前序号减一：请求先记，紧接着才是它的响应。
        let seq = self.seq.load(Ordering::Relaxed).saturating_sub(1);
        self.append(
            "response",
            seq,
            json!({
                "status": resp.status,
                "headers": redact(&resp.headers),
                "body": parse_body(&resp.body),
            }),
        );
    }
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
