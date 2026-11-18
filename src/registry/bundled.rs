//! 内置的 models.dev 快照 —— 联网拉不到时的兜底。
//!
//! `~/.carter/models.json` 缺失（首次运行、网络不通、models.dev 403）时，之前的行为是
//! 直接报错让用户先跑 `carter update`；国内网络下这一步经常卡住，用户连第一句话都发
//! 不出去。这里把发版时的快照压进二进制：有缓存用缓存，没缓存用快照，两者都是同一份
//! JSON 形态，下游解析不用分辨来源。
//!
//! 快照只保留 carter 读的字段（见 `scripts/refresh-models.py`），gzip 后三百多 KB。
//! 发版前跑一次脚本刷新，让快照不至于太旧。

use std::io::Read;
use std::sync::OnceLock;

static BUNDLED_GZ: &[u8] = include_bytes!("../../assets/models.json.gz");

/// 解压后的内置快照（进程内解一次）。
pub fn bundled_json() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| {
        let mut out = String::new();
        // 解不开只可能是构建产物损坏；给空对象让上游以「找不到模型」报错，而不是 panic。
        let _ = flate2::read::GzDecoder::new(BUNDLED_GZ).read_to_string(&mut out);
        if out.is_empty() {
            out.push_str("{}");
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_snapshot_is_valid_and_has_major_providers() {
        let json = bundled_json();
        let root: serde_json::Value = serde_json::from_str(json).expect("内置快照应是合法 JSON");
        for p in ["anthropic", "openai", "google", "deepseek"] {
            assert!(root.get(p).is_some(), "内置快照应包含 {p}");
        }
        assert!(super::super::models_dev::lookup(json, "anthropic", "claude-sonnet-4-5").is_some());
    }
}
