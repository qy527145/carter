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

/// 快照是哪天生成的（`YYYY-MM-DD`）。界面用它告诉用户「你用的是哪天的目录」,
/// 否则没法判断该不该联网刷新一次。
pub fn bundled_date() -> Option<&'static str> {
    static DATE: OnceLock<Option<String>> = OnceLock::new();
    DATE.get_or_init(|| {
        let root: serde_json::Value = serde_json::from_str(bundled_json()).ok()?;
        let date = root.get("$generated_at")?.as_str()?;
        Some(date.to_string())
    })
    .as_deref()
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

    /// 界面要显示「内置快照（x 月 y 日）」—— 没这个日期，用户不知道手里的目录
    /// 是刚拉的还是半年前发版带进来的。
    #[test]
    fn bundled_snapshot_carries_its_generation_date() {
        let d = bundled_date().expect("快照里应有 $generated_at");
        assert_eq!(d.len(), 10, "应形如 YYYY-MM-DD，实际是 {d}");
        assert_eq!(d.as_bytes()[4], b'-');
    }

    /// 快照要带上「按 base_url 认服务商」与「转售条目指回原厂」要用的字段 ——
    /// 宿主靠它们判断该去哪家查元数据、定价该按哪家算。
    #[test]
    fn bundled_snapshot_keeps_endpoint_and_canonical_fields() {
        let root: serde_json::Value = serde_json::from_str(bundled_json()).unwrap();
        let with_api = root
            .as_object()
            .unwrap()
            .iter()
            .filter(|(k, p)| !k.starts_with('$') && p.get("api").is_some())
            .count();
        assert!(with_api > 100, "带 api 字段的服务商太少：{with_api}");
        let with_canonical = root["deepseek"]["models"]
            .as_object()
            .unwrap()
            .values()
            .any(|m| m.get("canonical_model_id").is_some() || m.get("status").is_some());
        assert!(with_canonical, "快照里应保留 canonical_model_id / status");
    }
}
