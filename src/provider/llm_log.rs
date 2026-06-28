//! 大模型请求日志 —— 独立于 carter.log，按天拆分的 JSONL。
//! 默认目录 `~/.carter/debug/llm_log/<YYYY-MM-DD>.jsonl`（UTC 日期）。
//! **一行 = 一次完整交换**：`{ts,id,provider,request{...},response{...}}`。
//! 请求含 endpoint/method/model/system/messages/tools；响应把流式 SSE 合并成
//! text/thinking/tool_calls/usage/stop。用 `serde_json::to_string`（默认不转义非 ASCII，
//! 中文原样写入）。失败仅 warn 不影响请求。
//!
//! 说明：这是 carter 视角的「归一化交换」，非逐字节 wire 报文——精确的 HTTP 头/原始体
//! 由 genai 内部序列化，需要逐字节时用 mitmproxy 等抓包工具。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::session::now_ms;

/// 写入一条交换记录（已组装好的 JSON）。按 UTC 日期落到 `<dir>/<YYYY-MM-DD>.jsonl`。
pub fn write_exchange(dir: &Path, entry: &serde_json::Value) {
    let ms = now_ms();
    let (y, m, d, ..) = ymd_hms(ms);
    let file = dir.join(format!("{y:04}-{m:02}-{d:02}.jsonl"));
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("llm_log: create dir failed: {e}");
        return;
    }
    let line = match serde_json::to_string(entry) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("llm_log: serialize failed: {e}");
            return;
        }
    };
    match OpenOptions::new().create(true).append(true).open(&file) {
        Ok(mut f) => {
            let _ = writeln!(f, "{line}");
        }
        Err(e) => tracing::warn!("llm_log: open {} failed: {e}", file.display()),
    }
}

/// epoch ms → ISO-8601 UTC 字符串（无依赖）。
pub fn iso_utc(ms: u64) -> String {
    let (y, mo, d, h, mi, s, milli) = ymd_hms(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// epoch ms → (年, 月, 日, 时, 分, 秒, 毫秒)，UTC。
fn ymd_hms(ms: u64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let h = (rem / 3600) as u32;
    let mi = ((rem % 3600) / 60) as u32;
    let s = (rem % 60) as u32;
    (y, mo, d, h, mi, s, (ms % 1000) as u32)
}

/// 自 1970-01-01 的天数 → (年, 月, 日)。Howard Hinnant 的 civil_from_days 算法。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (y + if m <= 2 { 1 } else { 0 }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_632), (2026, 6, 28));
    }

    #[test]
    fn iso_utc_formats() {
        assert_eq!(iso_utc(0), "1970-01-01T00:00:00.000Z");
        let ms = (86_400 + 3661) * 1000 + 500;
        assert_eq!(iso_utc(ms), "1970-01-02T01:01:01.500Z");
    }

    #[test]
    fn write_exchange_keeps_utf8_unescaped() {
        let dir = std::env::temp_dir().join(format!("carter-llmlog-{}", now_ms()));
        let entry = serde_json::json!({
            "id": "x",
            "request": { "messages": [{"User": "你好"}] },
            "response": { "text": "世界" },
        });
        write_exchange(&dir, &entry);
        let mut found = false;
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let content = std::fs::read_to_string(ent.path()).unwrap();
                assert!(content.contains("你好") && content.contains("世界"), "原样中文: {content}");
                assert!(!content.contains("\\u4f60"), "不应转义: {content}");
                // 一行一条。
                assert_eq!(content.lines().count(), 1);
                found = true;
            }
        }
        assert!(found, "应生成日志文件");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
